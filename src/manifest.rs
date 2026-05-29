use std::{collections::HashMap, fs, path::Path};

use serde_json::Value;
use thiserror::Error;

use crate::strings::StringsCatalog;

#[derive(Debug, Clone)]
pub struct WallpaperAsset {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub url: String,
    pub file_name: String,
    pub extension: String,
}

#[derive(Debug, Clone)]
pub struct WallpaperSubcategory {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub assets: Vec<WallpaperAsset>,
}

#[derive(Debug, Clone)]
pub struct WallpaperCategory {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub subcategories: Vec<WallpaperSubcategory>,
}

#[derive(Debug, Clone)]
pub struct WallpaperCatalog {
    pub categories: Vec<WallpaperCategory>,
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to read manifest: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse manifest JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("manifest did not contain any downloadable assets")]
    Empty,
}

pub fn load_manifest(
    path: &Path,
    strings: Option<&StringsCatalog>,
) -> Result<WallpaperCatalog, ManifestError> {
    let raw = fs::read_to_string(path)?;
    let json: Value = serde_json::from_str(&raw)?;
    parse_catalog(&json, strings).ok_or(ManifestError::Empty)
}

fn parse_catalog(json: &Value, strings: Option<&StringsCatalog>) -> Option<WallpaperCatalog> {
    if json.get("categories").is_some() && json.get("assets").is_some() {
        parse_structured_catalog(json, strings)
    } else {
        parse_heuristic_catalog(json, strings)
    }
}

fn parse_structured_catalog(
    json: &Value,
    strings: Option<&StringsCatalog>,
) -> Option<WallpaperCatalog> {
    let categories_json = json.get("categories")?.as_array()?;
    let assets_json = json.get("assets")?.as_array()?;

    let mut category_lookup: HashMap<String, usize> = HashMap::new();
    let mut subcategory_lookup: HashMap<String, (usize, usize)> = HashMap::new();
    let mut catalog = WallpaperCatalog {
        categories: Vec::new(),
    };

    for category_value in categories_json {
        let category_object = category_value.as_object()?;
        let category_id = string_field(category_object, &["id"])?;
        let category_name = resolve_label(
            strings,
            string_field(category_object, &["localizedNameKey"]),
            string_field(
                category_object,
                &["localizedName", "name", "title", "displayName"],
            ),
        );
        let category_description = string_field(
            category_object,
            &[
                "localizedDescription",
                "localizedDescriptionKey",
                "description",
            ],
        );

        let cat_index = catalog.categories.len();
        category_lookup.insert(category_id.clone(), cat_index);
        let mut category = WallpaperCategory {
            id: category_id,
            name: category_name,
            description: category_description,
            subcategories: Vec::new(),
        };

        if let Some(subcategories) = category_object
            .get("subcategories")
            .and_then(|v| v.as_array())
        {
            for sub_value in subcategories {
                let sub_object = sub_value.as_object()?;
                let sub_id = string_field(sub_object, &["id"])?;
                let sub_name = resolve_label(
                    strings,
                    string_field(sub_object, &["localizedNameKey"]),
                    string_field(
                        sub_object,
                        &["localizedName", "name", "title", "displayName"],
                    ),
                );
                let sub_description = string_field(
                    sub_object,
                    &[
                        "localizedDescription",
                        "localizedDescriptionKey",
                        "description",
                    ],
                );
                let sub_index = category.subcategories.len();
                subcategory_lookup.insert(sub_id.clone(), (cat_index, sub_index));
                category.subcategories.push(WallpaperSubcategory {
                    id: sub_id,
                    name: sub_name,
                    description: sub_description,
                    assets: Vec::new(),
                });
            }
        }

        catalog.categories.push(category);
    }

    let mut unassigned_assets = Vec::new();
    for asset_value in assets_json {
        let asset_object = asset_value.as_object()?;
        let asset = extract_asset(asset_object, strings)?;

        if let Some((cat_idx, sub_idx)) =
            resolve_asset_target(asset_object, &category_lookup, &subcategory_lookup)
        {
            if let Some(subcategory) = catalog
                .categories
                .get_mut(cat_idx)
                .and_then(|category| category.subcategories.get_mut(sub_idx))
            {
                subcategory.assets.push(asset);
                continue;
            }
        }

        unassigned_assets.push(asset);
    }

    if !unassigned_assets.is_empty() {
        if catalog.categories.is_empty() {
            catalog.categories.push(WallpaperCategory {
                id: "general".to_owned(),
                name: "General".to_owned(),
                description: None,
                subcategories: vec![WallpaperSubcategory {
                    id: "all".to_owned(),
                    name: "All Wallpapers".to_owned(),
                    description: None,
                    assets: unassigned_assets,
                }],
            });
        } else {
            let category = &mut catalog.categories[0];
            if category.subcategories.is_empty() {
                category.subcategories.push(WallpaperSubcategory {
                    id: "all".to_owned(),
                    name: "All Wallpapers".to_owned(),
                    description: None,
                    assets: Vec::new(),
                });
            }
            category.subcategories[0].assets.extend(unassigned_assets);
        }
    }

    if catalog.categories.is_empty()
        || catalog
            .categories
            .iter()
            .all(|c| c.subcategories.is_empty())
    {
        return None;
    }

    Some(catalog)
}

fn parse_heuristic_catalog(
    json: &Value,
    strings: Option<&StringsCatalog>,
) -> Option<WallpaperCatalog> {
    let grouped_root = json.get("assets").unwrap_or(json);
    let mut grouped_assets = Vec::new();
    collect_grouped_assets(grouped_root, strings, &mut Vec::new(), &mut grouped_assets);

    if grouped_assets.is_empty() {
        return None;
    }

    let mut categories: Vec<WallpaperCategory> = Vec::new();
    let mut category_index_by_name: HashMap<String, usize> = HashMap::new();

    for (path, asset) in grouped_assets {
        let category_name = path
            .first()
            .cloned()
            .unwrap_or_else(|| "General".to_owned());
        let subcategory_name = if path.len() > 1 {
            path[1..].join(" / ")
        } else {
            "All Wallpapers".to_owned()
        };

        let category_idx = *category_index_by_name
            .entry(category_name.clone())
            .or_insert_with(|| {
                let idx = categories.len();
                categories.push(WallpaperCategory {
                    id: category_name.to_lowercase(),
                    name: resolve_label(strings, None, Some(category_name.clone())),
                    description: None,
                    subcategories: Vec::new(),
                });
                idx
            });

        let category = categories.get_mut(category_idx)?;
        let sub_idx = category
            .subcategories
            .iter()
            .position(|sub| sub.name == subcategory_name)
            .unwrap_or_else(|| {
                let idx = category.subcategories.len();
                category.subcategories.push(WallpaperSubcategory {
                    id: subcategory_name.to_lowercase(),
                    name: resolve_label(strings, None, Some(subcategory_name.clone())),
                    description: None,
                    assets: Vec::new(),
                });
                idx
            });

        category.subcategories[sub_idx].assets.push(asset);
    }

    Some(WallpaperCatalog { categories })
}

fn resolve_asset_target(
    asset_object: &serde_json::Map<String, Value>,
    category_lookup: &HashMap<String, usize>,
    subcategory_lookup: &HashMap<String, (usize, usize)>,
) -> Option<(usize, usize)> {
    if let Some(subcategory_id) = ref_id(asset_object, "subcategories") {
        if let Some(target) = subcategory_lookup.get(&subcategory_id) {
            return Some(*target);
        }
    }

    if let Some(category_id) = ref_id(asset_object, "categories") {
        if let Some(category_idx) = category_lookup.get(&category_id) {
            return Some((*category_idx, 0));
        }
    }

    None
}

fn ref_id(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    let value = map.get(key)?;
    let array = value.as_array()?;
    let first = array.first()?;
    match first {
        Value::Object(object) => string_field(object, &["id", "categoryID", "subcategoryID"]),
        Value::String(text) => Some(text.to_owned()),
        _ => None,
    }
}

fn extract_asset(
    map: &serde_json::Map<String, Value>,
    strings: Option<&StringsCatalog>,
) -> Option<WallpaperAsset> {
    let id = string_field(map, &["id"])?;
    let url = pick_url(map)?;
    let extension = extension_from_url(&url);
    let file_name = format!("{}{}", sanitize_filename(&id), extension);
    let title = resolve_label(
        strings,
        string_field(map, &["localizedNameKey"]),
        string_field(
            map,
            &[
                "title",
                "name",
                "displayName",
                "accessibilityLabel",
                "localizedName",
            ],
        ),
    );
    let description = string_field(
        map,
        &[
            "description",
            "subtitle",
            "storyline",
            "localizedDescription",
            "localizedDescriptionKey",
            "category",
        ],
    );

    Some(WallpaperAsset {
        id,
        title,
        description,
        url,
        file_name,
        extension,
    })
}

fn pick_url(map: &serde_json::Map<String, Value>) -> Option<String> {
    let preferred = [
        "url-4K-SDR-240FPS",
        "url-4K-HDR-240FPS",
        "url-4K-SDR",
        "url-4K-HDR",
        "url-HD",
        "url",
        "downloadURL",
    ];

    for key in preferred {
        if let Some(url) = string_field(map, &[key]) {
            return Some(url);
        }
    }

    for (key, value) in map {
        if key.starts_with("url-") {
            if let Some(url) = value.as_str() {
                return Some(url.to_owned());
            }
        }
    }

    None
}

fn extension_from_url(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    let candidate = path.rsplit('/').next().unwrap_or(path);
    let ext = Path::new(candidate)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mov");
    format!(".{}", ext)
}

fn string_field(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = map.get(*key) {
            if let Some(text) = value.as_str() {
                if !text.trim().is_empty() {
                    return Some(text.to_owned());
                }
            }
        }
    }
    None
}

fn resolve_label(
    strings: Option<&StringsCatalog>,
    localized_name_key: Option<String>,
    fallback: Option<String>,
) -> String {
    if let (Some(strings), Some(key)) = (strings, localized_name_key.as_deref()) {
        if let Some(value) = strings.lookup(key) {
            if !value.trim().is_empty() {
                return value.to_owned();
            }
        }
    }

    fallback
        .as_deref()
        .map(normalize_label)
        .unwrap_or_else(|| "General".to_owned())
}

fn normalize_label(raw: &str) -> String {
    let trimmed = raw
        .strip_prefix("AerialCategory")
        .or_else(|| raw.strip_prefix("AerialSubcategory"))
        .or_else(|| raw.strip_prefix("Aerial"))
        .unwrap_or(raw);

    let mut out = String::new();
    let mut prev_lower = false;
    for ch in trimmed.chars() {
        if ch.is_ascii_uppercase() && prev_lower {
            out.push(' ');
        }
        out.push(ch);
        prev_lower = ch.is_ascii_lowercase();
    }

    let out = out.replace('_', " ");
    let cleaned = out.trim();
    if cleaned.is_empty() {
        "General".to_owned()
    } else {
        cleaned.to_owned()
    }
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let replacement = match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        };
        out.push(replacement);
    }

    out.trim_matches('_').to_owned()
}

fn collect_grouped_assets<'a>(
    value: &'a Value,
    strings: Option<&StringsCatalog>,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, WallpaperAsset)>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_grouped_assets(item, strings, path, out);
            }
        }
        Value::Object(map) => {
            if let Some(asset) = extract_asset(map, strings) {
                out.push((path.clone(), asset));
                return;
            }

            for (key, child) in map {
                if is_metadata_key(key) {
                    continue;
                }

                let mut next_path = path.clone();
                if !is_container_key(key) {
                    next_path.push(key.clone());
                }
                collect_grouped_assets(child, strings, &mut next_path, out);
            }
        }
        _ => {}
    }
}

fn is_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "assets"
            | "items"
            | "children"
            | "entries"
            | "data"
            | "taxonomy"
            | "subcategories"
            | "categories"
            | "localizedName"
            | "localizedNameKey"
            | "representativeAssetID"
            | "id"
            | "name"
            | "title"
            | "description"
            | "subtitle"
            | "storyline"
            | "category"
            | "displayName"
            | "accessibilityLabel"
    )
}

fn is_container_key(key: &str) -> bool {
    matches!(
        key,
        "assets" | "items" | "children" | "entries" | "data" | "categories" | "subcategories"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strings::StringsCatalog;

    #[test]
    fn normalize_label_strips_aerial_category_prefix() {
        assert_eq!(normalize_label("AerialCategoryNature"), "Nature");
    }

    #[test]
    fn normalize_label_strips_aerial_subcategory_prefix() {
        assert_eq!(normalize_label("AerialSubcategoryDesert"), "Desert");
    }

    #[test]
    fn normalize_label_splits_camel_case_words() {
        assert_eq!(normalize_label("NorthAmerica"), "North America");
    }

    #[test]
    fn normalize_label_returns_general_for_empty() {
        assert_eq!(normalize_label(""), "General");
    }

    #[test]
    fn sanitize_filename_replaces_path_separator() {
        assert_eq!(sanitize_filename("foo/bar"), "foo_bar");
    }

    #[test]
    fn sanitize_filename_trims_leading_underscores() {
        assert_eq!(sanitize_filename("/leading"), "leading");
    }

    #[test]
    fn sanitize_filename_replaces_colon_and_asterisk() {
        assert_eq!(sanitize_filename("foo:bar*baz"), "foo_bar_baz");
    }

    #[test]
    fn extension_from_url_ignores_query_params() {
        assert_eq!(
            extension_from_url("https://cdn.example.com/wallpaper.mov?token=abc&expire=123"),
            ".mov"
        );
    }

    #[test]
    fn extension_from_url_defaults_to_mov_when_no_ext() {
        assert_eq!(
            extension_from_url("https://cdn.example.com/wallpaper"),
            ".mov"
        );
    }

    #[test]
    fn pick_url_prefers_4k_sdr_240fps_over_4k_sdr() {
        let json = serde_json::json!({
            "url-4K-SDR": "https://example.com/4ksdr.mov",
            "url-4K-SDR-240FPS": "https://example.com/4ksdr240.mov",
        });
        assert_eq!(
            pick_url(json.as_object().unwrap()).unwrap(),
            "https://example.com/4ksdr240.mov"
        );
    }

    #[test]
    fn pick_url_falls_back_to_download_url() {
        let json = serde_json::json!({
            "downloadURL": "https://example.com/download.mov"
        });
        assert_eq!(
            pick_url(json.as_object().unwrap()).unwrap(),
            "https://example.com/download.mov"
        );
    }

    #[test]
    fn pick_url_returns_none_when_no_url_present() {
        let json = serde_json::json!({"title": "no url here", "id": "abc"});
        assert!(pick_url(json.as_object().unwrap()).is_none());
    }

    #[test]
    fn structured_catalog_routes_assets_to_correct_subcategories() {
        let json = serde_json::json!({
            "categories": [
                {
                    "id": "cat1",
                    "name": "Nature",
                    "subcategories": [
                        {"id": "sub1", "name": "Forests"},
                        {"id": "sub2", "name": "Oceans"}
                    ]
                }
            ],
            "assets": [
                {
                    "id": "asset1",
                    "title": "Forest Scene",
                    "subcategories": [{"id": "sub1"}],
                    "url-4K-SDR": "https://example.com/forest.mov"
                },
                {
                    "id": "asset2",
                    "title": "Ocean Waves",
                    "subcategories": [{"id": "sub2"}],
                    "url-4K-SDR": "https://example.com/ocean.mov"
                }
            ]
        });
        let catalog = parse_structured_catalog(&json, None).unwrap();
        assert_eq!(catalog.categories[0].subcategories[0].assets[0].title, "Forest Scene");
        assert_eq!(catalog.categories[0].subcategories[1].assets[0].title, "Ocean Waves");
    }

    #[test]
    fn unassigned_assets_create_fallback_general_category() {
        let json = serde_json::json!({
            "categories": [],
            "assets": [
                {
                    "id": "stray1",
                    "title": "Stray Asset",
                    "url-HD": "https://example.com/stray.mov"
                }
            ]
        });
        let catalog = parse_structured_catalog(&json, None).unwrap();
        assert_eq!(catalog.categories[0].name, "General");
        assert_eq!(catalog.categories[0].subcategories[0].name, "All Wallpapers");
        assert_eq!(catalog.categories[0].subcategories[0].assets[0].title, "Stray Asset");
    }

    #[test]
    fn extracts_grouped_assets() {
        let json = serde_json::json!({
            "assets": {
                "Nature": {
                    "Mountains": [
                        {
                            "id": "summit",
                            "title": "Summit",
                            "url-4K-SDR": "https://example.com/summit.mov",
                            "description": "Sample asset"
                        }
                    ]
                }
            }
        });

        let catalog = parse_heuristic_catalog(&json, None).expect("catalog");
        assert_eq!(catalog.categories.len(), 1);
        assert_eq!(catalog.categories[0].name, "Nature");
        assert_eq!(catalog.categories[0].subcategories[0].name, "Mountains");
        assert_eq!(
            catalog.categories[0].subcategories[0].assets[0].title,
            "Summit"
        );
    }

    #[test]
    fn filename_uses_asset_id() {
        let json = serde_json::json!({
            "id": "abc123",
            "url-4K-SDR": "https://example.com/some.mov"
        });
        let asset = extract_asset(json.as_object().unwrap(), None).unwrap();
        assert_eq!(asset.file_name, "abc123.mov");
        assert_eq!(asset.extension, ".mov");
    }

    #[test]
    fn resolves_labels_from_strings_bundle() {
        let strings = StringsCatalog::from_pairs(&[
            ("CitiesKey", "Cityscape"),
            ("SpaceKey", "Earth"),
            ("CityAssetKey", "City skyline"),
        ]);
        let json = serde_json::json!({
            "categories": [
                {
                    "id": "cities",
                    "localizedNameKey": "CitiesKey",
                    "subcategories": [
                        {
                            "id": "space",
                            "localizedNameKey": "SpaceKey"
                        }
                    ]
                }
            ],
            "assets": [
                {
                    "id": "asset1",
                    "localizedNameKey": "CityAssetKey",
                    "categories": [{"id": "cities"}],
                    "url-4K-SDR": "https://example.com/asset1.mov"
                }
            ]
        });

        let catalog = parse_structured_catalog(&json, Some(&strings)).expect("catalog");
        assert_eq!(catalog.categories[0].name, "Cityscape");
        assert_eq!(catalog.categories[0].subcategories[0].name, "Earth");
        assert_eq!(
            catalog.categories[0].subcategories[0].assets[0].title,
            "City skyline"
        );
    }
}
