//! Checks of the node's own engine (`STORM_NODE:9090`). Its API is closed
//! (v17) and a Job is handed no token (stormcos#89), so only the public
//! health probe is always possible; the rest needs `STORM_STORMBLOCK_TOKEN`.

use std::time::Duration;

use serde_json::Value;
use stormblock::http::Client;

use crate::env::Env;
use crate::report::{Outcome, Why};

fn base(node: &str) -> String {
    let host = node.trim_start_matches("http://").trim_end_matches('/');
    // The node's address, or the address and a port already.
    let has_port = host.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !host.ends_with(']');
    if has_port {
        format!("http://{host}")
    } else {
        format!("http://{host}:9090")
    }
}

async fn get(url: &str, token: Option<&str>) -> Result<(u16, Value), String> {
    let c = Client::builder()
        .timeout(Duration::from_secs(10))
        .bearer(token.map(str::to_string))
        .build()
        .map_err(|e| e.to_string())?;
    let r = c.get(url).send().await.map_err(|e| e.to_string())?;
    let s = r.status().as_u16();
    let t = r.text().await.unwrap_or_default();
    Ok((s, serde_json::from_str(&t).unwrap_or(Value::Null)))
}

/// The node's engine is up: its public health probe answers, and says the
/// API is closed.
pub async fn health(env: &Env) -> Outcome {
    let Some(node) = env.node.as_deref() else {
        return Err(Why::Skip("STORM_NODE is not set".into()));
    };
    let url = format!("{}/api/v1/health", base(node));
    match get(&url, None).await {
        Err(e) => Err(Why::Skip(format!("no stormblock answering at {url}: {e}"))),
        Ok((200, v)) => {
            if v["auth"] == "none" {
                return Err(Why::Fail(format!("the node's engine is open (auth=none): {v}")));
            }
            Ok(format!("version {} auth {}", v["version"], v["auth"]))
        }
        Ok((s, v)) => Err(Why::Fail(format!("{url} answered {s}: {v}"))),
    }
}

/// With a token: the node's engine lists its volumes and slabs.
pub async fn inventory(env: &Env) -> Outcome {
    let (Some(node), Some(token)) = (env.node.as_deref(), env.node_token.as_deref()) else {
        return Err(Why::Skip("no STORM_STORMBLOCK_TOKEN for the node's engine (stormcos#89)".into()));
    };
    let b = base(node);
    let (s, vols) = get(&format!("{b}/api/v1/volumes"), Some(token)).await?;
    if s != 200 {
        return Err(Why::Fail(format!("GET volumes answered {s}")));
    }
    let (s, slabs) = get(&format!("{b}/api/v1/slabs"), Some(token)).await?;
    if s != 200 {
        return Err(Why::Fail(format!("GET slabs answered {s}")));
    }
    let n = |v: &Value| v["count"].as_u64().or(v.as_array().map(|a| a.len() as u64)).unwrap_or(0);
    Ok(format!("{} volume(s) on {} slab(s)", n(&vols), n(&slabs)))
}
