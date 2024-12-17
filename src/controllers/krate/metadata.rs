//! Endpoints that expose metadata about a crate
//!
//! These endpoints provide data that could be obtained directly from the
//! index or cached metadata which was extracted (client side) from the
//! `Cargo.toml` file.

use crate::app::AppState;
use crate::controllers::krate::CratePath;
use crate::models::{
    Category, Crate, CrateCategory, CrateKeyword, Keyword, RecentCrateDownloads, User, Version,
    VersionOwnerAction,
};
use crate::schema::*;
use crate::util::errors::{bad_request, crate_not_found, AppResult, BoxedAppError};
use crate::util::RequestUtils;
use crate::views::{EncodableCategory, EncodableCrate, EncodableKeyword, EncodableVersion};
use axum_extra::json;
use axum_extra::response::ErasedJson;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use http::request::Parts;
use std::cmp::Reverse;
use std::str::FromStr;

/// Get crate metadata (for the `new` crate).
///
/// This endpoint works around a small limitation in `axum` and is delegating
/// to the `GET /api/v1/crates/{name}` endpoint internally.
#[utoipa::path(
    get,
    path = "/api/v1/crates/new",
    tag = "crates",
    responses((status = 200, description = "Successful Response")),
)]
pub async fn find_new_crate(app: AppState, req: Parts) -> AppResult<ErasedJson> {
    let name = "new".to_string();
    find_crate(app, CratePath { name }, req).await
}

/// Get crate metadata.
#[utoipa::path(
    get,
    path = "/api/v1/crates/{name}",
    params(CratePath),
    tag = "crates",
    responses((status = 200, description = "Successful Response")),
)]
pub async fn find_crate(app: AppState, path: CratePath, req: Parts) -> AppResult<ErasedJson> {
    let mut conn = app.db_read().await?;

    let include = req
        .query()
        .get("include")
        .map(|mode| ShowIncludeMode::from_str(mode))
        .transpose()?
        .unwrap_or_default();

    let (krate, downloads, default_version, yanked): (Crate, i64, Option<String>, Option<bool>) =
        Crate::by_name(&path.name)
            .inner_join(crate_downloads::table)
            .left_join(default_versions::table)
            .left_join(versions::table.on(default_versions::version_id.eq(versions::id)))
            .select((
                Crate::as_select(),
                crate_downloads::downloads,
                versions::num.nullable(),
                versions::yanked.nullable(),
            ))
            .first(&mut conn)
            .await
            .optional()?
            .ok_or_else(|| crate_not_found(&path.name))?;

    let versions_publishers_and_audit_actions = if include.versions {
        let mut versions_and_publishers: Vec<(Version, Option<User>)> =
            Version::belonging_to(&krate)
                .left_outer_join(users::table)
                .select(<(Version, Option<User>)>::as_select())
                .load(&mut conn)
                .await?;

        versions_and_publishers
            .sort_by_cached_key(|(version, _)| Reverse(semver::Version::parse(&version.num).ok()));

        let versions = versions_and_publishers
            .iter()
            .map(|(v, _)| v)
            .collect::<Vec<_>>();
        let actions = VersionOwnerAction::for_versions(&mut conn, &versions).await?;
        Some(
            versions_and_publishers
                .into_iter()
                .zip(actions)
                .map(|((v, pb), aas)| (v, pb, aas))
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    let ids = versions_publishers_and_audit_actions
        .as_ref()
        .map(|vps| vps.iter().map(|v| v.0.id).collect());

    let kws = if include.keywords {
        Some(
            CrateKeyword::belonging_to(&krate)
                .inner_join(keywords::table)
                .select(Keyword::as_select())
                .load(&mut conn)
                .await?,
        )
    } else {
        None
    };
    let cats = if include.categories {
        Some(
            CrateCategory::belonging_to(&krate)
                .inner_join(categories::table)
                .select(Category::as_select())
                .load(&mut conn)
                .await?,
        )
    } else {
        None
    };
    let recent_downloads = if include.downloads {
        RecentCrateDownloads::belonging_to(&krate)
            .select(recent_crate_downloads::downloads)
            .get_result(&mut conn)
            .await
            .optional()?
    } else {
        None
    };

    let top_versions = if include.versions {
        Some(krate.top_versions(&mut conn).await?)
    } else {
        None
    };

    let encodable_crate = EncodableCrate::from(
        krate.clone(),
        default_version.as_deref(),
        yanked,
        top_versions.as_ref(),
        ids,
        kws.as_deref(),
        cats.as_deref(),
        false,
        downloads,
        recent_downloads,
    );

    let encodable_versions = versions_publishers_and_audit_actions.map(|vpa| {
        vpa.into_iter()
            .map(|(v, pb, aas)| EncodableVersion::from(v, &krate.name, pb, aas))
            .collect::<Vec<_>>()
    });

    let encodable_keywords = kws.map(|kws| {
        kws.into_iter()
            .map(Keyword::into)
            .collect::<Vec<EncodableKeyword>>()
    });

    let encodable_cats = cats.map(|cats| {
        cats.into_iter()
            .map(Category::into)
            .collect::<Vec<EncodableCategory>>()
    });

    Ok(json!({
        "crate": encodable_crate,
        "versions": encodable_versions,
        "keywords": encodable_keywords,
        "categories": encodable_cats,
    }))
}

#[derive(Debug)]
struct ShowIncludeMode {
    versions: bool,
    keywords: bool,
    categories: bool,
    badges: bool,
    downloads: bool,
}

impl Default for ShowIncludeMode {
    fn default() -> Self {
        // Send everything for legacy clients that expect the full response
        Self {
            versions: true,
            keywords: true,
            categories: true,
            badges: true,
            downloads: true,
        }
    }
}

impl ShowIncludeMode {
    const INVALID_COMPONENT: &'static str =
        "invalid component for ?include= (expected 'versions', 'keywords', 'categories', 'badges', 'downloads', or 'full')";
}

impl FromStr for ShowIncludeMode {
    type Err = BoxedAppError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut mode = Self {
            versions: false,
            keywords: false,
            categories: false,
            badges: false,
            downloads: false,
        };
        for component in s.split(',') {
            match component {
                "" => {}
                "full" => {
                    mode = Self {
                        versions: true,
                        keywords: true,
                        categories: true,
                        badges: true,
                        downloads: true,
                    }
                }
                "versions" => mode.versions = true,
                "keywords" => mode.keywords = true,
                "categories" => mode.categories = true,
                "badges" => mode.badges = true,
                "downloads" => mode.downloads = true,
                _ => return Err(bad_request(Self::INVALID_COMPONENT)),
            }
        }
        Ok(mode)
    }
}
