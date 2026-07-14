//! `state clear | show <adapter>` — the KV escape hatch (§13).
//!
//! Operates directly on an adapter's KV bucket (the bucket name *is* the adapter
//! name). `show` dumps every key/value; `clear` deletes them all.

use anyhow::{Context, Result};

use crate::app::App;
use crate::output;

/// `state clear <adapter>`: delete every key in the adapter's bucket.
pub fn clear(app: &App, adapter: &str) -> Result<u8> {
    let store = app.open_store()?;
    let bucket = store.bucket(adapter);
    let keys = bucket
        .keys("")
        .with_context(|| format!("reading bucket '{adapter}'"))?;
    for key in &keys {
        bucket
            .delete(key)
            .with_context(|| format!("deleting '{key}' from bucket '{adapter}'"))?;
    }

    if app.json {
        output::print_json(&serde_json::json!({
            "adapter": adapter,
            "cleared": keys.len(),
        }))?;
    } else {
        println!("cleared {} key(s) from bucket '{adapter}'", keys.len());
    }
    Ok(0)
}

/// `state show <adapter>`: dump every key/value in the adapter's bucket.
pub fn show(app: &App, adapter: &str) -> Result<u8> {
    let store = app.open_store()?;
    let bucket = store.bucket(adapter);
    let pairs = bucket
        .iter::<serde_json::Value>("")
        .with_context(|| format!("reading bucket '{adapter}'"))?;

    if app.json {
        let map: serde_json::Map<String, serde_json::Value> = pairs.into_iter().collect();
        output::print_json(&serde_json::Value::Object(map))?;
    } else if pairs.is_empty() {
        println!("bucket '{adapter}' is empty");
    } else {
        for (key, value) in &pairs {
            println!("{key} = {value}");
        }
    }
    Ok(0)
}
