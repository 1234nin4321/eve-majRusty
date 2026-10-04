//! Jita ore prices from the public ESI API, for the config dialog's ore table.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::http_client::{FetchOptions, HttpClient};
use crate::log::Scope;

const SLOG: Scope = Scope::new("esi_prices");

const ESI_BASE: &str = "https://esi.evetech.net/latest";
const ESI_JITA_REGION_ID: i64 = 10000002;
const ESI_JITA_STATION_ID: i64 = 60003760;

/// Builds the POST body for /universe/ids/: each ore name as "Compressed <name>".
fn ore_ids_request_body(names: &[String]) -> Vec<u8> {
    let prefixed: Vec<String> = names.iter().map(|name| format!("Compressed {name}")).collect();
    serde_json::to_vec(&prefixed).unwrap_or_default()
}

/// Picks "Compressed <name>" -> type_id out of an /universe/ids/ response, keyed by the uncompressed name.
fn parse_ore_type_ids(body: &[u8]) -> BTreeMap<String, i64> {
    let mut result = BTreeMap::new();
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(err) => {
            SLOG.warn(format_args!("Failed to parse ESI universe/ids response: {err}"));
            return result;
        }
    };

    let Value::Object(object) = &parsed else {
        SLOG.warn(format_args!("ESI universe/ids response was not a JSON object: {}", String::from_utf8_lossy(body)));
        return result;
    };
    let Some(inventory_types) = object.get("inventory_types") else {
        SLOG.warn(format_args!("ESI universe/ids response had no inventory_types field: {}", String::from_utf8_lossy(body)));
        return result;
    };
    let Value::Array(items) = inventory_types else {
        SLOG.warn(format_args!("ESI universe/ids inventory_types was not an array: {}", String::from_utf8_lossy(body)));
        return result;
    };

    for item in items {
        let Value::Object(item) = item else { continue };
        let (Some(id_val), Some(name_val)) = (item.get("id"), item.get("name")) else { continue };
        let (Some(id), Some(name)) = (id_val.as_i64(), name_val.as_str()) else { continue };

        let Some(base_name) = name.strip_prefix("Compressed ") else { continue };
        result.insert(base_name.to_owned(), id);
    }

    result
}

/// Resolves "Compressed <name>" -> ESI type_id for each ore name via the public (no-auth) ESI name resolver.
/// Names with no market match are simply absent from the result.
fn resolve_ore_type_ids<F>(fetch: &F, names: &[String]) -> BTreeMap<String, i64>
where
    F: Fn(&str, &FetchOptions<'_>) -> Option<Vec<u8>>,
{
    if names.is_empty() {
        return BTreeMap::new();
    }

    let body = ore_ids_request_body(names);
    let options = FetchOptions { content_type: Some("application/json"), payload: Some(&body), ..Default::default() };
    let Some(response) = fetch(&format!("{ESI_BASE}/universe/ids/?datasource=tranquility"), &options) else {
        return BTreeMap::new();
    };
    parse_ore_type_ids(&response)
}

/// Highest Jita 4-4 buy price in a /markets/{region}/orders/ response, or null if there is none.
fn parse_jita_buy_price(body: &[u8], type_id: i64) -> Option<f64> {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(err) => {
            SLOG.warn(format_args!("Failed to parse ESI price response for type_id {type_id}: {err}"));
            return None;
        }
    };
    let Value::Array(orders) = &parsed else { return None };

    let mut best: Option<f64> = None;
    for order in orders {
        let Value::Object(order) = order else { continue };
        let Some(location_val) = order.get("location_id") else { continue };
        if location_val.as_i64() != Some(ESI_JITA_STATION_ID) {
            continue;
        }

        let Some(price_val) = order.get("price") else { continue };
        let price = if let Some(i) = price_val.as_i64() {
            i as f64
        } else if price_val.is_f64() {
            price_val.as_f64().unwrap_or_default()
        } else {
            continue;
        };
        if best.is_none_or(|b| price > b) {
            best = Some(price);
        }
    }
    best
}

fn jita_buy_orders_url(type_id: i64) -> String {
    format!("{ESI_BASE}/markets/{ESI_JITA_REGION_ID}/orders/?datasource=tranquility&order_type=buy&type_id={type_id}")
}

/// Highest current Jita 4-4 buy order price for type_id (what a seller would instantly receive), or null if unavailable/illiquid.
/// Only reads page 1 of the region's buy orders - fine for these commodity ore types, whose buy-order counts stay well under the 1000-order page size in practice.
fn fetch_jita_buy_price<F>(fetch: &F, type_id: i64) -> Option<f64>
where
    F: Fn(&str, &FetchOptions<'_>) -> Option<Vec<u8>>,
{
    let Some(body) = fetch(&jita_buy_orders_url(type_id), &FetchOptions::default()) else {
        SLOG.warn(format_args!("ESI price fetch failed for type_id {type_id}"));
        return None;
    };
    parse_jita_buy_price(&body, type_id)
}

/// Caps how many HTTP requests run at once for a price fetch - bounded so this stays polite to ESI rather than opening dozens of connections at once.
const MAX_CONCURRENT_PRICE_REQUESTS: usize = 8;

struct PriceLookup {
    name: String,
    type_id: i64,
}

struct PriceFetchContext<'a, F> {
    fetch: &'a F,
    lookups: &'a [PriceLookup],
    next_index: AtomicUsize,
    results: Mutex<Map<String, Value>>,
}

/// Pulls lookups off ctx's shared index until exhausted; safe to run on several threads (including the caller's) at once.
fn price_fetch_worker<F>(ctx: &PriceFetchContext<'_, F>)
where
    F: Fn(&str, &FetchOptions<'_>) -> Option<Vec<u8>>,
{
    loop {
        let i = ctx.next_index.fetch_add(1, Ordering::Relaxed);
        if i >= ctx.lookups.len() {
            return;
        }

        let lookup = &ctx.lookups[i];
        let Some(price) = fetch_jita_buy_price(ctx.fetch, lookup.type_id) else { continue };

        let mut results = ctx.results.lock().unwrap_or_else(|p| p.into_inner());
        match serde_json::Number::from_f64(price) {
            Some(n) => {
                results.insert(lookup.name.clone(), Value::Number(n));
            }
            None => SLOG.warn(format_args!("Failed to store price for {}: non-finite value", lookup.name)),
        }
    }
}

/// `fetch_ore_prices` with the HTTP request injected, so the request flow is testable without the network.
pub fn fetch_ore_prices_with<F>(request: &str, fetch: &F) -> String
where
    F: Fn(&str, &FetchOptions<'_>) -> Option<Vec<u8>> + Sync,
{
    let names: Vec<String> = match serde_json::from_str(request) {
        Ok(n) => n,
        Err(err) => {
            SLOG.warn(format_args!("Failed to parse fetchOrePrices request: {err}"));
            return "{}".to_owned();
        }
    };

    let type_ids = resolve_ore_type_ids(fetch, &names);
    let lookups: Vec<PriceLookup> = type_ids.into_iter().map(|(name, type_id)| PriceLookup { name, type_id }).collect();

    let ctx = PriceFetchContext { fetch, lookups: &lookups, next_index: AtomicUsize::new(0), results: Mutex::new(Map::new()) };

    if !lookups.is_empty() {
        // Spawn up to MAX_CONCURRENT_PRICE_REQUESTS - 1 background workers; the calling thread pulls from the same queue as the last one, so a failed spawn just means less parallelism, not less work done.
        let worker_count = MAX_CONCURRENT_PRICE_REQUESTS.min(lookups.len());
        std::thread::scope(|scope| {
            for _ in 0..worker_count - 1 {
                if let Err(err) = std::thread::Builder::new().spawn_scoped(scope, || price_fetch_worker(&ctx)) {
                    SLOG.warn(format_args!("Failed to spawn price-fetch worker: {err}"));
                }
            }
            price_fetch_worker(&ctx);
        });
    }

    let results = ctx.results.into_inner().unwrap_or_else(|p| p.into_inner());
    match serde_json::to_string(&Value::Object(results)) {
        Ok(json) => json,
        Err(err) => {
            SLOG.warn(format_args!("Failed to serialize ESI price response: {err}"));
            "{}".to_owned()
        }
    }
}

/// Looks up each ore name's Jita buy price via its compressed variant (readily liquid there) using the public ESI API - no key required.
/// Request body: JSON array of ore names. Response: JSON object of {name: price}, omitting names with no market match.
pub fn fetch_ore_prices(request: &str) -> String {
    let client = HttpClient::new();
    fetch_ore_prices_with(request, &|url: &str, options: &FetchOptions<'_>| client.fetch(url, options))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS_URL: &str = "https://esi.evetech.net/latest/universe/ids/?datasource=tranquility";

    #[test]
    fn request_body_prefixes_each_name() {
        let body = ore_ids_request_body(&["Veldspar".into(), "Dark Ochre".into()]);
        assert_eq!(body, br#"["Compressed Veldspar","Compressed Dark Ochre"]"#);
    }

    #[test]
    fn parses_compressed_type_ids_and_skips_everything_else() {
        let body = br#"{"inventory_types":[
            {"id":62516,"name":"Compressed Veldspar"},
            {"id":1230,"name":"Veldspar"},
            {"id":"62517","name":"Compressed Scordite"},
            {"id":1.5,"name":"Compressed Pyroxeres"},
            {"name":"Compressed Kernite"},
            7
        ],"systems":[]}"#;
        let ids = parse_ore_type_ids(body);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids.get("Veldspar"), Some(&62516));
    }

    #[test]
    fn malformed_ids_responses_resolve_nothing() {
        assert!(parse_ore_type_ids(b"not json").is_empty());
        assert!(parse_ore_type_ids(b"[]").is_empty());
        assert!(parse_ore_type_ids(b"{}").is_empty());
        assert!(parse_ore_type_ids(br#"{"inventory_types":{}}"#).is_empty());
    }

    #[test]
    fn best_buy_price_is_the_highest_at_jita_4_4() {
        let body = br#"[
            {"location_id":60003760,"price":101.5,"is_buy_order":true},
            {"location_id":60003760,"price":120},
            {"location_id":60008494,"price":999.0},
            {"location_id":60003760,"price":"500"},
            {"location_id":60003760},
            "junk"
        ]"#;
        assert_eq!(parse_jita_buy_price(body, 1), Some(120.0));
        assert_eq!(parse_jita_buy_price(b"[]", 1), None);
        assert_eq!(parse_jita_buy_price(b"{}", 1), None);
        assert_eq!(parse_jita_buy_price(b"oops", 1), None);
    }

    #[test]
    fn price_url_targets_the_forge_buy_orders() {
        assert_eq!(
            jita_buy_orders_url(62516),
            "https://esi.evetech.net/latest/markets/10000002/orders/?datasource=tranquility&order_type=buy&type_id=62516"
        );
    }

    #[test]
    fn fetch_ore_prices_combines_id_and_price_lookups() {
        let fetch = |url: &str, options: &FetchOptions<'_>| -> Option<Vec<u8>> {
            if url == IDS_URL {
                assert_eq!(options.content_type, Some("application/json"));
                assert_eq!(options.payload, Some(&br#"["Compressed Veldspar","Compressed Scordite","Compressed Unobtainium"]"#[..]));
                return Some(br#"{"inventory_types":[{"id":1,"name":"Compressed Veldspar"},{"id":2,"name":"Compressed Scordite"}]}"#.to_vec());
            }
            assert!(options.payload.is_none());
            if url.ends_with("type_id=1") {
                Some(br#"[{"location_id":60003760,"price":12.5}]"#.to_vec())
            } else {
                None
            }
        };
        let out = fetch_ore_prices_with(r#"["Veldspar","Scordite","Unobtainium"]"#, &fetch);
        assert_eq!(out, r#"{"Veldspar":12.5}"#);
    }

    #[test]
    fn fetch_ore_prices_handles_many_lookups_concurrently() {
        let names: Vec<String> = (0..30).map(|i| format!("Ore{i}")).collect();
        let fetch = |url: &str, _: &FetchOptions<'_>| -> Option<Vec<u8>> {
            if url == IDS_URL {
                let types: Vec<Value> =
                    (0..30).map(|i| serde_json::json!({"id": i, "name": format!("Compressed Ore{i}")})).collect();
                return Some(serde_json::to_vec(&serde_json::json!({ "inventory_types": types })).unwrap());
            }
            let id: i64 = url.rsplit('=').next().unwrap().parse().unwrap();
            Some(format!(r#"[{{"location_id":60003760,"price":{id}.25}}]"#).into_bytes())
        };
        let out = fetch_ore_prices_with(&serde_json::to_string(&names).unwrap(), &fetch);
        let parsed: Map<String, Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.len(), 30);
        assert_eq!(parsed["Ore7"].as_f64(), Some(7.25));
    }

    #[test]
    fn bad_requests_and_failed_lookups_return_an_empty_object() {
        let never = |_: &str, _: &FetchOptions<'_>| -> Option<Vec<u8>> { panic!("no request expected") };
        assert_eq!(fetch_ore_prices_with("not json", &never), "{}");
        assert_eq!(fetch_ore_prices_with("[]", &never), "{}");
        let failing = |_: &str, _: &FetchOptions<'_>| -> Option<Vec<u8>> { None };
        assert_eq!(fetch_ore_prices_with(r#"["Veldspar"]"#, &failing), "{}");
    }
}
