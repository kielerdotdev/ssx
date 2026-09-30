//! Sparse-package manifest for the Windows 11 top-level context menu.
//!
//! The COM DLL (`IExplorerCommand`) is **not implemented yet**; see
//! `crates/ssx-shell/docs/windows11-context-menu.md` for the design. What exists now is the
//! manifest text generator, so the package layout, CLSIDs and verb list are pinned down and
//! unit-tested (golden file + XML well-formedness).
//!
//! CLSIDs are derived from the action id, so they are stable across releases: changing a CLSID
//! would orphan the previous registration.

use crate::action::{Action, FilterKind};
use crate::error::{Result, ShellError};
use crate::quote::{push_fmt, xml_escape};

/// Inputs for [`sparse_package_manifest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparsePackageParams {
    /// `Identity/@Name`, e.g. `Ssx.ShellExtension`.
    pub package_name: String,
    /// `Identity/@Publisher`; must equal the signing certificate subject, e.g. `CN=ssx`.
    pub publisher: String,
    /// Shown in Settings > Apps.
    pub publisher_display_name: String,
    /// Package display name.
    pub display_name: String,
    /// Four-part version `a.b.c.d`.
    pub version: String,
    /// Extension DLL file name, relative to the external location (install dir).
    pub dll_file: String,
    /// The ssx executable (relative to the external location) that owns package identity.
    pub exe_file: String,
    /// `x64` or `arm64`.
    pub architecture: String,
    /// Entries to expose as verbs.
    pub actions: Vec<Action>,
}

impl SparsePackageParams {
    /// Defaults matching the ssx release layout.
    pub fn ssx_defaults(version: &str) -> Self {
        Self {
            package_name: "Ssx.ShellExtension".into(),
            publisher: "CN=ssx".into(),
            publisher_display_name: "ssx".into(),
            display_name: "ssx".into(),
            version: version.into(),
            dll_file: "ssx_shell_ext.dll".into(),
            exe_file: "ssx.exe".into(),
            architecture: "x64".into(),
            actions: Action::defaults(),
        }
    }
}

/// Stable GUID (`{8-4-4-4-12}`, version-5 style bits) derived from `seed`.
///
/// FNV-1a over two salted passes; not cryptographic, only needs to be stable and well spread.
pub fn guid_for(seed: &str) -> String {
    fn fnv(seed: &str, salt: u8) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ u64::from(salt);
        for b in seed.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }
    let (a, b) = (fnv(seed, 1), fnv(seed, 2));
    let d1 = (a >> 32) as u32;
    let d2 = (a >> 16) as u16;
    let d3 = ((a as u16) & 0x0fff) | 0x5000;
    let d4 = (((b >> 48) as u16) & 0x3fff) | 0x8000;
    let d5 = b & 0xffff_ffff_ffff;
    format!("{{{d1:08X}-{d2:04X}-{d3:04X}-{d4:04X}-{d5:012X}}}")
}

/// CLSID of the `IExplorerCommand` class implementing `action`.
pub fn clsid_for(action: &Action) -> String {
    guid_for(&format!("ssx-shell:explorer-command:{}", action.id))
}

/// Verb id used in the manifest for `action`.
pub fn verb_id(action: &Action) -> String {
    let mut s = String::from("Ssx");
    for part in action.id.split('-') {
        let mut c = part.chars();
        if let Some(f) = c.next() {
            s.push(f.to_ascii_uppercase());
            s.push_str(c.as_str());
        }
    }
    s
}

fn item_types(a: &Action) -> Vec<String> {
    let mut v = Vec::new();
    match a.filter.kind {
        FilterKind::Any => v.push("*".to_owned()),
        FilterKind::Images | FilterKind::Videos | FilterKind::Custom => {
            if a.filter.extensions.is_empty() {
                v.push("*".to_owned());
            }
            v.extend(a.filter.extensions.iter().map(|e| format!(".{e}")));
        }
    }
    if a.filter.directories {
        v.push("Directory".to_owned());
    }
    v
}

fn validate(p: &SparsePackageParams) -> Result<()> {
    let bad =
        |what: &str| ShellError::Unavailable(format!("invalid sparse package parameter: {what}"));
    let name_ok = !p.package_name.is_empty()
        && p.package_name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !name_ok {
        return Err(bad("package_name must be [A-Za-z0-9.-]+"));
    }
    let parts: Vec<&str> = p.version.split('.').collect();
    if parts.len() != 4 || parts.iter().any(|x| x.parse::<u16>().is_err()) {
        return Err(bad("version must be four numbers a.b.c.d (each <= 65535)"));
    }
    if !matches!(p.architecture.as_str(), "x64" | "arm64" | "x86") {
        return Err(bad("architecture must be x64, arm64 or x86"));
    }
    for f in [&p.dll_file, &p.exe_file] {
        if f.is_empty()
            || f.contains(['/', '\\', '"', '<', '>', ':'])
            || f.chars().any(char::is_control)
        {
            return Err(bad("dll_file/exe_file must be plain file names"));
        }
    }
    for s in [&p.publisher, &p.publisher_display_name, &p.display_name] {
        if s.is_empty() || s.chars().any(char::is_control) {
            return Err(bad("publisher/display names must be non-empty single-line text"));
        }
    }
    for a in &p.actions {
        a.validate()?;
    }
    Ok(())
}

/// Generates `AppxManifest.xml` for the sparse package.
///
/// Key points (see the design doc for the reasoning): `uap10:AllowExternalContent` lets the
/// package live beside the unpackaged app; `desktop4:FileExplorerContextMenus` +
/// `desktop5:ItemType/Verb` map item types to COM classes; `com:SurrogateServer` runs the DLL
/// out of process in `dllhost.exe` so a crash cannot take Explorer down.
pub fn sparse_package_manifest(p: &SparsePackageParams) -> Result<String> {
    validate(p)?;
    let e = xml_escape;
    let mut verbs = String::new();
    // Group verbs by item type, preserving first-seen order.
    let mut groups: Vec<(String, Vec<&Action>)> = Vec::new();
    for a in &p.actions {
        for t in item_types(a) {
            match groups.iter_mut().find(|(g, _)| *g == t) {
                Some((_, v)) => v.push(a),
                None => groups.push((t, vec![a])),
            }
        }
    }
    for (t, actions) in &groups {
        push_fmt!(verbs, "            <desktop5:ItemType Type=\"{}\">\n", e(t));
        for a in actions {
            push_fmt!(
                verbs,
                "              <desktop5:Verb Id=\"{}\" Clsid=\"{}\" />\n",
                e(&verb_id(a)),
                clsid_for(a)
            );
        }
        verbs.push_str("            </desktop5:ItemType>\n");
    }
    let mut classes = String::new();
    for a in &p.actions {
        push_fmt!(
            classes,
            "                <com:Class Id=\"{}\" Path=\"{}\" ThreadingModel=\"STA\" />\n",
            clsid_for(a).trim_matches(['{', '}']),
            e(&p.dll_file)
        );
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!-- ssx-shell-managed: generated by ssx-shell::windows::manifest; do not edit by hand. -->
<Package
  xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
  xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
  xmlns:uap10="http://schemas.microsoft.com/appx/manifest/uap/windows10/10"
  xmlns:desktop4="http://schemas.microsoft.com/appx/manifest/desktop/windows10/4"
  xmlns:desktop5="http://schemas.microsoft.com/appx/manifest/desktop/windows10/5"
  xmlns:com="http://schemas.microsoft.com/appx/manifest/com/windows10"
  xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
  IgnorableNamespaces="uap uap10 desktop4 desktop5 com rescap">
  <Identity Name="{name}" Publisher="{publisher}" Version="{version}" ProcessorArchitecture="{arch}" />
  <Properties>
    <DisplayName>{display}</DisplayName>
    <PublisherDisplayName>{pub_display}</PublisherDisplayName>
    <Logo>Assets\StoreLogo.png</Logo>
    <uap10:AllowExternalContent>true</uap10:AllowExternalContent>
  </Properties>
  <Resources>
    <Resource Language="en-us" />
  </Resources>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.22000.0" MaxVersionTested="10.0.26100.0" />
  </Dependencies>
  <Capabilities>
    <rescap:Capability Name="runFullTrust" />
  </Capabilities>
  <Applications>
    <Application Id="Ssx" Executable="{exe}" uap10:TrustLevel="mediumIL" uap10:RuntimeBehavior="win32App">
      <uap:VisualElements
        AppListEntry="none"
        DisplayName="{display}"
        Description="{display} file manager integration"
        BackgroundColor="transparent"
        Square150x150Logo="Assets\Square150x150Logo.png"
        Square44x44Logo="Assets\Square44x44Logo.png" />
      <Extensions>
        <desktop4:Extension Category="windows.fileExplorerContextMenus">
          <desktop4:FileExplorerContextMenus>
{verbs}          </desktop4:FileExplorerContextMenus>
        </desktop4:Extension>
        <com:Extension Category="windows.comServer">
          <com:ComServer>
            <com:SurrogateServer DisplayName="{display} context menu">
{classes}            </com:SurrogateServer>
          </com:ComServer>
        </com:Extension>
      </Extensions>
    </Application>
  </Applications>
</Package>
"#,
        name = e(&p.package_name),
        publisher = e(&p.publisher),
        version = e(&p.version),
        arch = e(&p.architecture),
        display = e(&p.display_name),
        pub_display = e(&p.publisher_display_name),
        exe = e(&p.exe_file),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_is_stable_and_well_formed() {
        let g = guid_for("x");
        assert_eq!(g, guid_for("x"));
        assert_ne!(g, guid_for("y"));
        assert_eq!(g.len(), 38);
        assert!(g.starts_with('{') && g.ends_with('}'));
        let parts: Vec<&str> = g[1..37].split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('5'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'A' | b'B'));
    }

    #[test]
    fn clsids_differ_per_action() {
        let ids: Vec<String> = Action::defaults().iter().map(clsid_for).collect();
        let mut dedup = ids.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), ids.len());
    }

    #[test]
    fn validation() {
        let mut p = SparsePackageParams::ssx_defaults("1.2.3.0");
        assert!(sparse_package_manifest(&p).is_ok());
        p.version = "1.2.3".into();
        assert!(sparse_package_manifest(&p).is_err());
        let mut p = SparsePackageParams::ssx_defaults("1.2.3.0");
        p.package_name = "bad name".into();
        assert!(sparse_package_manifest(&p).is_err());
        let mut p = SparsePackageParams::ssx_defaults("1.2.3.0");
        p.dll_file = r"..\evil.dll".into();
        assert!(sparse_package_manifest(&p).is_err());
    }

    #[test]
    fn verb_ids() {
        assert_eq!(verb_id(&Action::upload_video()), "SsxUploadVideo");
    }
}
