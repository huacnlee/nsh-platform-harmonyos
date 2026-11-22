//! Signs an OpenHarmony HAP with the OpenHarmony SDK's own debug keys
//! (`toolchains/lib/OpenHarmony.p12`), as DevEco Studio's "automatically
//! generate signature" does without an account: an app certificate issued
//! by the keystore's application CA, a release profile for the bundle name
//! (valid on any device, so no device id is needed), and `hap-sign-tool`.
//! HarmonyOS devices take only Huawei-issued signatures; this is for
//! OpenHarmony devices and emulators.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, bail};
use serde_json::Value;

/// The SDK keystore's password (the same for its keys), published with it.
const PASSWORD: &str = "123456";
const APP_KEY: &str = "openharmony application release";
const PROFILE_KEY: &str = "openharmony application profile release";
const APP_CA: &str = "openharmony application ca";
const ROOT_CA: &str = "openharmony application root ca";

/// Signs `unsigned` into `signed` for `bundle`, with the keys in `lib`
/// (`sdk::signing_lib`). `work` holds the certificates and profile.
pub(crate) fn sign(
    lib: &Path,
    work: &Path,
    bundle: &str,
    unsigned: &Path,
    signed: &Path,
    verbose: bool,
) -> anyhow::Result<()> {
    fs::create_dir_all(work)?;
    let keystore = lib.join("OpenHarmony.p12");
    let tool = lib.join("hap-sign-tool.jar");
    let run = |cmd: &mut Command, what: &str| -> anyhow::Result<()> {
        if !verbose {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let status = cmd
            .status()
            .with_context(|| format!("running {what} (it comes with a Java runtime)"))?;
        if !status.success() {
            bail!("{what} failed (run with -v to see why)");
        }
        Ok(())
    };
    let sign_tool = |command: &str| {
        let mut cmd = Command::new(java("java"));
        cmd.arg("-jar").arg(&tool).arg(command);
        cmd
    };

    for (alias, file) in [(APP_CA, "subCA.cer"), (ROOT_CA, "rootCA.cer")] {
        let mut cmd = Command::new(java("keytool"));
        cmd.args([
            "-exportcert",
            "-rfc",
            "-storetype",
            "pkcs12",
            "-storepass",
            PASSWORD,
        ])
        .arg("-keystore")
        .arg(&keystore)
        .args(["-alias", alias, "-file"])
        .arg(work.join(file));
        run(&mut cmd, "keytool")?;
    }
    let app_cert = work.join("app.pem");
    let mut cmd = sign_tool("generate-app-cert");
    cmd.args(["-keyAlias", APP_KEY, "-signAlg", "SHA256withECDSA"])
        .args([
            "-issuer",
            "C=CN,O=OpenHarmony,OU=OpenHarmony Team,CN=OpenHarmony Application CA",
        ])
        .args(["-issuerKeyAlias", APP_CA])
        .args([
            "-subject",
            "C=CN,O=OpenHarmony,OU=OpenHarmony Team,CN=OpenHarmony Application Release",
        ])
        .arg("-keystoreFile")
        .arg(&keystore)
        .args([
            "-keystorePwd",
            PASSWORD,
            "-keyPwd",
            PASSWORD,
            "-issuerKeyPwd",
            PASSWORD,
        ])
        .arg("-subCaCertFile")
        .arg(work.join("subCA.cer"))
        .arg("-rootCaCertFile")
        .arg(work.join("rootCA.cer"))
        .args(["-outForm", "certChain", "-validity", "3650", "-outFile"])
        .arg(&app_cert);
    run(&mut cmd, "hap-sign-tool generate-app-cert")?;

    let template = fs::read_to_string(lib.join("UnsgnedReleasedProfileTemplate.json"))?;
    let chain = fs::read_to_string(&app_cert)?;
    let profile = profile(&template, &chain, bundle, now())?;
    let profile_json = work.join("profile.json");
    fs::write(&profile_json, profile)?;
    let profile_p7b = work.join("profile.p7b");
    let mut cmd = sign_tool("sign-profile");
    cmd.args([
        "-keyAlias",
        PROFILE_KEY,
        "-signAlg",
        "SHA256withECDSA",
        "-mode",
        "localSign",
    ])
    .arg("-profileCertFile")
    .arg(lib.join("OpenHarmonyProfileRelease.pem"))
    .arg("-inFile")
    .arg(&profile_json)
    .arg("-keystoreFile")
    .arg(&keystore)
    .args(["-keystorePwd", PASSWORD, "-keyPwd", PASSWORD, "-outFile"])
    .arg(&profile_p7b);
    run(&mut cmd, "hap-sign-tool sign-profile")?;

    let mut cmd = sign_tool("sign-app");
    cmd.args([
        "-keyAlias",
        APP_KEY,
        "-signAlg",
        "SHA256withECDSA",
        "-mode",
        "localSign",
    ])
    .arg("-appCertFile")
    .arg(&app_cert)
    .arg("-profileFile")
    .arg(&profile_p7b)
    .arg("-inFile")
    .arg(unsigned)
    .arg("-keystoreFile")
    .arg(&keystore)
    .args([
        "-keystorePwd",
        PASSWORD,
        "-keyPwd",
        PASSWORD,
        "-signCode",
        "1",
        "-outFile",
    ])
    .arg(signed);
    run(&mut cmd, "hap-sign-tool sign-app")
}

/// The SDK's release profile template for `bundle`: distributed by the app
/// certificate (the chain's first), valid ten years from a day before `now`.
fn profile(template: &str, chain: &str, bundle: &str, now: u64) -> anyhow::Result<String> {
    let end = "-----END CERTIFICATE-----";
    let leaf = chain
        .find(end)
        .map(|i| format!("{}\n", &chain[..i + end.len()]))
        .context("the app certificate has no certificate")?;
    let mut json: Value = serde_json::from_str(template).context("the SDK's profile template")?;
    json["bundle-info"]["distribution-certificate"] = Value::String(leaf);
    json["bundle-info"]["bundle-name"] = Value::String(bundle.to_owned());
    json["uuid"] = Value::String(uuid(now));
    json["validity"] = serde_json::json!({
        "not-before": now.saturating_sub(86_400),
        "not-after": now + 3650 * 86_400,
    });
    Ok(serde_json::to_string_pretty(&json)?)
}

/// A UUID-shaped id: only unique, never verified.
fn uuid(now: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let a = now ^ (u64::from(std::process::id()) << 32);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        a as u32,
        (a >> 32) as u16,
        nanos & 0xfff,
        (nanos >> 12) & 0xfff,
        u64::from(nanos) << 16 | (a & 0xffff)
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `java` or `keytool` from `JAVA_HOME`, or the one on `PATH`.
fn java(tool: &str) -> PathBuf {
    let name = format!("{tool}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin").join(&name))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| name.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_the_profile_template() {
        let template = r#"{"version-name":"2.0.0","uuid":"x","validity":{},"bundle-info":{"bundle-name":"","distribution-certificate":""},"type":"release"}"#;
        let chain = "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nBBB\n-----END CERTIFICATE-----\n";
        let out: Value =
            serde_json::from_str(&profile(template, chain, "com.example.app", 1_000_000).unwrap())
                .unwrap();
        assert_eq!(out["bundle-info"]["bundle-name"], "com.example.app");
        let cert = out["bundle-info"]["distribution-certificate"]
            .as_str()
            .unwrap();
        assert!(cert.contains("AAA") && !cert.contains("BBB"), "{cert}");
        assert_eq!(out["validity"]["not-before"], 1_000_000 - 86_400);
        assert_eq!(out["type"], "release");
        assert_eq!(uuid(1).len(), 36);
    }
}
