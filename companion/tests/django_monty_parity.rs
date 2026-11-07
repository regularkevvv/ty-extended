//! Actual django-ty regression requests through the native embedded Monty host.
//! This tests runtime/protocol preservation; frontend compatibility is a separate
//! gate in scripts/django_acceptance.py.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, io::BufRead, path::PathBuf};
use ty_plugin_host::{
    HostError, MontyLimits, MontyRunner, PluginEnvironment, PluginHost, RuntimeError,
};
use ty_plugin_protocol::{PluginManifest, PluginRequest, PluginResponse};

#[test]
#[ignore = "requires DJANGO_TY_SOURCE and DJANGO_TY_ORACLE external regression fixtures"]
fn existing_django_responses_obey_production_eight_mib_limit() -> Result<()> {
    let limits = MontyLimits::default();
    ensure!(limits.max_response_bytes == 8 * 1024 * 1024);
    replay(limits, true)
}

#[test]
#[ignore = "requires DJANGO_TY_SOURCE and DJANGO_TY_ORACLE external regression fixtures"]
fn all_django_responses_match_with_explicit_sixteen_mib_limit() -> Result<()> {
    // The original plugin's direct-interpreter parity suite includes a roughly
    // 12 MiB stress response. Both shipped checker hosts default to 8 MiB, so
    // the test above requires rejection there. This separately configured host
    // tests the complete response serialization. It does not change production
    // defaults or imply that the stress response fits the shipped host limit.
    replay(
        MontyLimits {
            max_response_bytes: 16 * 1024 * 1024,
            ..MontyLimits::default()
        },
        false,
    )
}

fn replay(limits: MontyLimits, reject_oversized: bool) -> Result<()> {
    let source = PathBuf::from(std::env::var("DJANGO_TY_SOURCE").context("set DJANGO_TY_SOURCE")?);
    let oracle = PathBuf::from(std::env::var("DJANGO_TY_ORACLE").context("set DJANGO_TY_ORACLE")?);
    let package = source.join("python/django_ty");
    let manifest_value: Value =
        serde_json::from_slice(&fs::read(package.join("ty-plugin-monty.json"))?)?;
    let manifest: PluginManifest = serde_json::from_value(manifest_value.clone())?;
    let id = manifest.id.clone();
    let environment = PluginEnvironment::from_manifests(vec![manifest])?;
    let max_response_bytes = limits.max_response_bytes;
    let mut runner = MontyRunner::new(limits)?;
    runner.add_plugin(&id, fs::read(package.join("monty.py"))?)?;
    let host = PluginHost::new(environment, runner);
    let mut counts = BTreeMap::<String, usize>::new();
    let mut total = 0;
    let mut rejected = 0;
    for (index, line) in std::io::BufReader::new(fs::File::open(oracle)?)
        .lines()
        .enumerate()
    {
        let record: Value = serde_json::from_str(&line?)?;
        let request: PluginRequest = serde_json::from_value(record["request"].clone())?;
        let expected: PluginResponse = serde_json::from_value(record["expected"].clone())?;
        let expected_size = serde_json::to_vec(&expected)?.len();
        let response = host.execute(&id, &request);
        if reject_oversized && expected_size > max_response_bytes {
            ensure!(
                matches!(
                    response,
                    Err(HostError::Runtime {
                        source: RuntimeError::ResponseTooLarge,
                        ..
                    })
                ),
                "stress record {index} must hit the unchanged production response limit: {response:?}"
            );
            rejected += 1;
            total += 1;
            continue;
        }
        let actual = response.with_context(|| {
            format!(
                "embedded Monty regression record {index} ({}) with {} serialized expected response bytes",
                record["request"]["kind"],
                expected_size
            )
        })?;
        ensure!(
            actual == expected,
            "Monty response differs from Rust regression record {index}: request={} expected={} actual={}",
            record["request"],
            serde_json::to_string(&expected)?,
            serde_json::to_string(&actual)?
        );
        *counts
            .entry(
                record["request"]["kind"]
                    .as_str()
                    .context("request kind")?
                    .to_string(),
            )
            .or_default() += 1;
        total += 1;
    }
    ensure!(
        total >= 150,
        "regression corpus unexpectedly small: {total}"
    );
    ensure!(
        rejected == usize::from(reject_oversized),
        "expected one oversized stress fixture at the production bound, got {rejected}"
    );
    let manifest_request: PluginRequest = serde_json::from_value(json!({"kind":"manifest"}))?;
    let mut expected_manifest = manifest_value;
    expected_manifest["kind"] = json!("manifest");
    let expected_manifest: PluginResponse = serde_json::from_value(expected_manifest)?;
    ensure!(
        host.execute(&id, &manifest_request)? == expected_manifest,
        "embedded Monty manifest differs from the packaged manifest"
    );
    println!(
        "{total} Django regressions checked with {max_response_bytes} byte response limit ({rejected} expected oversized rejections), plus matching packaged manifest: {}",
        serde_json::to_string(&counts)?
    );
    Ok(())
}
