//! End-to-end check on a synthetic Skyrim SE folder: install -> conflicts -> deploy -> reorder
//! -> external change -> undo/redo -> uninstall/undo -> purge.
use rmm_core::{Game, Instance, conflicts, deploy, health, install, plugins};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Minimal valid Skyrim SE plugin: a TES4 record with HEDR and MAST/DATA pairs.
fn plugin_bytes(masters: &[&str], master_flag: bool) -> Vec<u8> {
    let mut sub = Vec::new();
    sub.extend(b"HEDR");
    sub.extend(12u16.to_le_bytes());
    sub.extend(1.71f32.to_le_bytes());
    sub.extend(0u32.to_le_bytes());
    sub.extend(0x800u32.to_le_bytes());
    for m in masters {
        sub.extend(b"MAST");
        sub.extend(((m.len() + 1) as u16).to_le_bytes());
        sub.extend(m.as_bytes());
        sub.push(0);
        sub.extend(b"DATA");
        sub.extend(8u16.to_le_bytes());
        sub.extend(0u64.to_le_bytes());
    }
    let mut rec = Vec::new();
    rec.extend(b"TES4");
    rec.extend((sub.len() as u32).to_le_bytes());
    rec.extend((master_flag as u32).to_le_bytes());
    rec.extend([0u8; 4]); // form id
    rec.extend([0u8; 4]); // version control
    rec.extend(44u16.to_le_bytes());
    rec.extend(0u16.to_le_bytes());
    rec.extend(sub);
    rec
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn read(path: &Path) -> String {
    String::from_utf8_lossy(&fs::read(path).unwrap()).into_owned()
}

#[test]
fn full_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let (game, local, src, root) = (tmp.path().join("game"), tmp.path().join("local"), tmp.path().join("src"), tmp.path().join("inst"));

    // Vanilla game.
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&game.join("Data/textures/sky.dds"), "vanilla");

    // Mod A: plain folder of Data files.
    write(&src.join("A/textures/sky.dds"), "A");
    write(&src.join("A/meshes/a.nif"), "a-mesh");
    write(&src.join("A/A.esp"), plugin_bytes(&["Skyrim.esm"], false));

    // Mod B: zip with one wrapper folder (the installer must descend into it).
    let zip_path = src.join("B-1-0.zip");
    {
        let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("B v1/textures/sky.dds", opts).unwrap();
        z.write_all(b"B").unwrap();
        z.start_file("B v1/B.esp", opts).unwrap();
        z.write_all(&plugin_bytes(&["Skyrim.esm", "A.esp"], false)).unwrap();
        z.finish().unwrap();
    }

    // Script-extender style archive: root files plus a Data folder.
    write(&src.join("skse/skse64_2/skse64_loader.exe"), "loader");
    write(&src.join("skse/skse64_2/Data/Scripts/skse.pex"), "pex");

    let inst = Instance::create(&root, Game::SkyrimSe, &game, &local).unwrap();
    let a = install::install(&inst, &src.join("A"), None, true, None).unwrap();
    let b = install::install(&inst, &zip_path, Some("B"), true, None).unwrap();
    let s = install::install(&inst, &src.join("skse"), Some("SKSE"), true, None).unwrap();
    assert_eq!(a.plugins, vec!["A.esp"]);
    assert_eq!(b.plugins, vec!["B.esp"]);
    assert_eq!((s.root_files, s.files), (1, 2));

    // B is installed last so it wins sky.dds; A is not fully overridden (it has a.nif).
    let c = conflicts::conflicts(&inst, &inst.profile().unwrap()).unwrap();
    let ca = c.iter().find(|c| c.id == "A").unwrap();
    assert_eq!((ca.losing_files, ca.overwritten_by.clone(), ca.fully_overridden), (1, vec!["B".to_string()], false));

    // Dry run touches nothing.
    let plan = deploy::deploy(&inst, true).unwrap();
    assert_eq!(plan.backup, vec!["Data/textures/sky.dds"]);
    assert_eq!(read(&game.join("Data/textures/sky.dds")), "vanilla");

    let plan = deploy::deploy(&inst, false).unwrap();
    assert_eq!(plan.link.len(), 6);
    assert_eq!(read(&game.join("Data/textures/sky.dds")), "B");
    assert_eq!(read(&game.join("skse64_loader.exe")), "loader");
    let txt = read(&local.join("plugins.txt"));
    assert!(txt.find("*A.esp").unwrap() < txt.find("*B.esp").unwrap(), "plugins.txt was: {txt}");
    assert!(deploy::deploy(&inst, true).unwrap().is_noop());

    // Reorder: A now wins. Incremental deploy only relinks the one conflicting file.
    inst.edit("move B up", |p| {
        let b = p.mods.iter().position(|m| m.id == "B").unwrap();
        let m = p.mods.remove(b);
        p.mods.insert(0, m);
        Ok(())
    })
    .unwrap();
    let plan = deploy::deploy(&inst, false).unwrap();
    assert_eq!(plan.link, vec!["Data/textures/sky.dds"]);
    assert_eq!(read(&game.join("Data/textures/sky.dds")), "A");

    // A tool replaces a deployed file (atomic save): captured to overwrite/, which wins from then on.
    fs::remove_file(game.join("Data/meshes/a.nif")).unwrap();
    write(&game.join("Data/meshes/a.nif"), "edited by tool");
    assert!(health::check(&inst).unwrap().iter().any(|i| i.kind == "external_change"));
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(read(&root.join("overwrite/meshes/a.nif")), "edited by tool");
    assert_eq!(read(&game.join("Data/meshes/a.nif")), "edited by tool");
    assert!(deploy::deploy(&inst, true).unwrap().is_noop());

    // Undo / redo the reorder.
    assert_eq!(inst.undo().unwrap().as_deref(), Some("move B up"));
    assert_eq!(inst.profile().unwrap().mods.last().unwrap().id, "SKSE");
    assert_eq!(inst.profile().unwrap().mods[1].id, "B");
    assert_eq!(inst.redo().unwrap().as_deref(), Some("move B up"));
    assert_eq!(inst.profile().unwrap().mods[0].id, "B");

    // Disabling A leaves B.esp without its master.
    inst.edit("disable A", |p| {
        p.mods.iter_mut().find(|m| m.id == "A").unwrap().enabled = false;
        Ok(())
    })
    .unwrap();
    let pl = plugins::list(&inst, &inst.profile().unwrap()).unwrap();
    assert_eq!(pl.iter().find(|p| p.name == "B.esp").unwrap().missing_masters, vec!["A.esp"]);
    assert!(health::check(&inst).unwrap().iter().any(|i| i.kind == "missing_masters"));
    inst.undo().unwrap();

    // Uninstall goes to trash; undo brings the files back.
    install::uninstall(&inst, "A").unwrap();
    assert!(!inst.mod_dir("A").exists());
    inst.undo().unwrap();
    assert!(inst.mod_dir("A").join("A.esp").exists());

    // Purge restores vanilla and removes everything else we created.
    deploy::purge(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/textures/sky.dds")), "vanilla");
    assert!(!game.join("Data/meshes").exists());
    assert!(!game.join("Data/Scripts").exists());
    assert!(!game.join("skse64_loader.exe").exists());
    assert!(game.join("Data/Skyrim.esm").exists());
    assert!(!game.join(".rmm-owner").exists());
}

/// Vanilla Skyrim folder plus one installed, deployed mod A (sky.dds over vanilla, a.nif new).
fn deployed_setup(tmp: &Path) -> (Instance, std::path::PathBuf) {
    let (game, root) = (tmp.join("game"), tmp.join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/textures/sky.dds"), "vanilla");
    write(&tmp.join("A/textures/sky.dds"), "A");
    write(&tmp.join("A/meshes/a.nif"), "a-mesh");
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &tmp.join("local")).unwrap();
    install::install(&inst, &tmp.join("A"), None, true, None).unwrap();
    deploy::deploy(&inst, false).unwrap();
    (inst, game)
}

fn has_issue(inst: &Instance, kind: &str) -> bool {
    health::check(inst).unwrap().iter().any(|i| i.kind == kind)
}

#[test]
fn changes_are_detected_by_file_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    let sky = game.join("Data/textures/sky.dds");

    // Replaced by a file with the same size and mtime: only identity can tell.
    let mtime = fs::metadata(&sky).unwrap().modified().unwrap();
    fs::remove_file(&sky).unwrap();
    write(&sky, "Z");
    fs::File::options().write(true).open(&sky).unwrap().set_modified(mtime).unwrap();
    let plan = deploy::deploy(&inst, false).unwrap();
    assert_eq!(plan.external_changes, vec!["Data/textures/sky.dds"]);
    assert_eq!(read(&inst.root.join("overwrite/textures/sky.dds")), "Z");
    assert_eq!(read(&sky), "Z");

    // Edited in place through the hard link: the mod file changed with it, nothing to capture.
    fs::write(game.join("Data/meshes/a.nif"), "edited in place").unwrap();
    assert!(deploy::deploy(&inst, true).unwrap().is_noop());

    // Files a tool drops into overwrite/ are deployed on top.
    write(&inst.root.join("overwrite/meshes/new.nif"), "tool output");
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/meshes/new.nif")), "tool output");
}

#[test]
fn purge_without_manifest_finds_links_by_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    fs::remove_file(inst.root.join("deploy.json")).unwrap();

    let err = deploy::deploy(&inst, true).unwrap_err().to_string();
    assert!(err.contains("deploy.json"), "{err}");
    assert!(has_issue(&inst, "deploy_blocked"));

    let mut removed = deploy::purge(&inst, true).unwrap().remove;
    removed.sort();
    assert_eq!(removed, vec!["Data/meshes/a.nif", "Data/textures/sky.dds"]);
    deploy::purge(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/textures/sky.dds")), "vanilla");
    assert!(!game.join("Data/meshes").exists());
    assert!(!game.join(".rmm-owner").exists());
    deploy::deploy(&inst, false).unwrap();
}

#[test]
fn deploy_refuses_a_vortex_managed_game() {
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    write(&game.join("Data/vortex.deployment.json"), "{}");
    assert!(deploy::deploy(&inst, true).unwrap_err().to_string().contains("Vortex"));
    assert!(has_issue(&inst, "deploy_blocked"));
}

#[cfg(windows)]
#[test]
fn deploy_refuses_while_the_game_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    let exe = game.join("SkyrimSE.exe");
    fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
    let mut child = std::process::Command::new(&exe).args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
    let blocked = deploy::deploy(&inst, false);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(blocked.unwrap_err().to_string().contains("running"));
    deploy::deploy(&inst, false).unwrap();
}

/// Calls the INI check directly: the full health check lists plugins through libloadorder, which
/// reads the real Documents\My Games\Fallout4 INIs on this machine.
#[test]
fn fallout4_ini_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let (game, ini) = (tmp.path().join("game"), tmp.path().join("My Games/Fallout4"));
    write(&game.join("Fallout4.exe"), "exe");
    write(&tmp.path().join("M/textures/x.dds"), "x");
    let mut inst = Instance::create(&tmp.path().join("inst"), Game::Fallout4, &game, &tmp.path().join("local")).unwrap();
    inst.config.ini_path = Some(ini.clone());
    install::install(&inst, &tmp.path().join("M"), None, true, None).unwrap();
    let kinds = |inst: &Instance| health::fallout4_ini(inst).unwrap().into_iter().map(|i| i.kind).collect::<Vec<_>>();

    assert!(kinds(&inst).is_empty(), "no INIs yet: the game has never run");
    write(&ini.join("Fallout4.ini"), "[General]\nsTestFile1=Foo.esp\n[Archive]\nsResourceDataDirsFinal=STRINGS\\\n");
    assert_eq!(kinds(&inst), vec!["ini_test_file", "archive_invalidation"]);

    write(&ini.join("Fallout4Custom.ini"), "\u{feff}[General]\nsTestFile1=\n[Archive]\nbInvalidateOlderFiles=1\nsResourceDataDirsFinal=\n");
    assert!(kinds(&inst).is_empty());
}

/// Steps are listed out of order (the schema sorts them by name). Picking Blue sets a flag that
/// reveals "2 Extras"; the default (Red) triggers a conditional patch. Green is NotUsable until
/// Core.esp is active. base/tex.dds has priority 5, so it beats either colour's tex.dds.
const MODULE_CONFIG: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:noNamespaceSchemaLocation="http://qconsulting.ca/fo3/ModConfig5.0.xsd">
  <moduleName>Test Mod</moduleName>
  <requiredInstallFiles>
    <file source="core\Core.esp" destination="Core.esp"/>
    <file source="docs\readme.txt"/>
    <file source="base\tex.dds" destination="textures\" priority="5"/>
    <folder source="" destination=""/>
  </requiredInstallFiles>
  <installSteps>
    <installStep name="2 Extras">
      <visible><flagDependency flag="colour" value="blue"/></visible>
      <optionalFileGroups>
        <group name="Extras" type="SelectAny">
          <plugins>
            <plugin name="Glow">
              <description>Glowing bits</description>
              <files><file source="extras\glow.dds" destination="textures\glow.dds"/></files>
              <typeDescriptor><type name="Optional"/></typeDescriptor>
            </plugin>
          </plugins>
        </group>
      </optionalFileGroups>
    </installStep>
    <installStep name="1 Colour">
      <optionalFileGroups>
        <group name="Colour" type="SelectExactlyOne">
          <plugins>
            <plugin name="Red">
              <description>Red textures</description>
              <files><folder source="red" destination="textures"/></files>
              <conditionFlags><flag name="colour">red</flag></conditionFlags>
              <typeDescriptor><type name="Recommended"/></typeDescriptor>
            </plugin>
            <plugin name="Blue">
              <description>Blue textures</description>
              <files><folder source="blue" destination="textures"/></files>
              <conditionFlags><flag name="colour">blue</flag></conditionFlags>
              <typeDescriptor><type name="Optional"/></typeDescriptor>
            </plugin>
            <plugin name="Green">
              <description>Needs Core.esp active</description>
              <files><folder source="blue" destination="textures"/></files>
              <typeDescriptor><dependencyType>
                <defaultType name="NotUsable"/>
                <patterns><pattern>
                  <dependencies><fileDependency file="Core.esp" state="Active"/></dependencies>
                  <type name="Optional"/>
                </pattern></patterns>
              </dependencyType></typeDescriptor>
            </plugin>
          </plugins>
        </group>
      </optionalFileGroups>
    </installStep>
  </installSteps>
  <conditionalFileInstalls><patterns>
    <pattern>
      <dependencies><flagDependency flag="colour" value="red"/></dependencies>
      <files><file source="patches\red_patch.esp" destination="RedPatch.esp"/></files>
    </pattern>
    <pattern>
      <dependencies operator="Or">
        <fileDependency file="Skyrim.esm" state="Active"/>
        <fileDependency file="Nope.esp" state="Active"/>
      </dependencies>
      <files><file source="patches\vanilla.txt" destination="vanilla.txt"/></files>
    </pattern>
  </patterns></conditionalFileInstalls>
</config>"#;

fn fomod_source(dir: &Path) {
    let xml: Vec<u8> = [0xFF, 0xFE].into_iter().chain(MODULE_CONFIG.encode_utf16().flat_map(u16::to_le_bytes)).collect();
    write(&dir.join("fomod/ModuleConfig.xml"), xml);
    write(&dir.join("core/Core.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&dir.join("docs/readme.txt"), "readme");
    write(&dir.join("base/tex.dds"), "base");
    write(&dir.join("red/tex.dds"), "red");
    write(&dir.join("red/sub/r.dds"), "r");
    write(&dir.join("blue/tex.dds"), "blue");
    write(&dir.join("extras/glow.dds"), "glow");
    write(&dir.join("patches/red_patch.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&dir.join("patches/vanilla.txt"), "v");
}

fn choices(v: serde_json::Value) -> rmm_core::fomod::Choices {
    serde_json::from_value(v).unwrap()
}

#[test]
fn fomod_installers() {
    use rmm_core::fomod::Kind;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let src = tmp.path().join("F wrapper/F");
    fomod_source(&src);
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();

    let err = install::install(&inst, &src, None, true, None).unwrap_err().to_string();
    assert!(err.contains("fomod_options"), "{err}");
    assert!(inst.profile().unwrap().mods.is_empty());

    // Defaults: Red is recommended, so "2 Extras" stays hidden and the red patch is added.
    let walk = install::fomod_options(&inst, &src, None).unwrap();
    assert_eq!(walk.module, "Test Mod");
    assert_eq!(walk.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["1 Colour"]);
    let opts = &walk.steps[0].groups[0].options;
    assert_eq!(opts.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["Blue", "Green", "Red"]);
    assert_eq!(opts.iter().map(|o| o.selected).collect::<Vec<_>>(), [false, false, true]);
    assert_eq!(opts[1].kind, Kind::NotUsable);
    assert_eq!(walk.files, 6, "{walk:#?}"); // Core.esp, readme, tex, sub/r, RedPatch.esp, vanilla.txt

    let blue = choices(serde_json::json!({"options": [{"name": "1 Colour", "groups": [{"name": "Colour", "choices": [{"name": "Blue"}]}]}]}));
    let walk = install::fomod_options(&inst, &src, Some(&blue)).unwrap();
    assert_eq!(walk.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["1 Colour", "2 Extras"]);

    let green = choices(serde_json::json!({"options": [{"name": "1 Colour", "groups": [{"name": "Colour", "choices": [{"name": "Green"}]}]}]}));
    assert!(format!("{:#}", install::fomod_options(&inst, &src, Some(&green)).unwrap_err()).contains("can't be used"));
    let two = choices(serde_json::json!({"options": [{"name": "1 Colour", "groups": [{"name": "Colour", "choices": [{"name": "Red"}, {"name": "Blue"}]}]}]}));
    assert!(format!("{:#}", install::fomod_options(&inst, &src, Some(&two)).unwrap_err()).contains("exactly one"));

    let mut picks = walk.choices.clone();
    picks.options[1].groups[0].choices.push(rmm_core::fomod::PluginChoice { name: "Glow".into(), idx: None });
    let r = install::install(&inst, &src, Some("Blue"), true, Some(&picks)).unwrap();
    let files = inst.mod_files(&r.id).unwrap();
    assert_eq!(files, ["Core.esp", "docs/readme.txt", "textures/glow.dds", "textures/tex.dds", "vanilla.txt"]);
    assert_eq!(read(&inst.mod_dir(&r.id).join("textures/tex.dds")), "base");
    assert_eq!(r.plugins, ["Core.esp"]);
    let meta = inst.mod_meta(&r.id).fomod.unwrap();
    assert_eq!(meta.options[1].groups[0].choices[0].name, "Glow");
    assert!(read(&inst.mod_dir(&r.id).join("rmm-mod.toml")).contains("type = \"fomod\""));

    // Zip: preview, then install with defaults from the same unpacked copy.
    let zip_path = tmp.path().join("F.zip");
    {
        let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        for e in walkdir::WalkDir::new(&src).into_iter().filter_map(Result::ok).filter(|e| e.file_type().is_file()) {
            let rel = e.path().strip_prefix(&src).unwrap().to_string_lossy().replace('\\', "/");
            z.start_file(rel, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(&fs::read(e.path()).unwrap()).unwrap();
        }
        z.finish().unwrap();
    }
    assert_eq!(install::fomod_options(&inst, &zip_path, None).unwrap().files, 6);
    let r = install::install(&inst, &zip_path, Some("Red"), true, Some(&Default::default())).unwrap();
    assert_eq!(r.plugins, ["Core.esp", "RedPatch.esp"]);
    assert_eq!(read(&inst.mod_dir(&r.id).join("textures/sub/r.dds")), "r");
    assert!(!inst.root.join(".staging/preview").exists());
}

/// Minimal Skyrim SE (v105) BSA with names and no file data: enough for listing.
fn bsa_bytes(files: &[&str]) -> Vec<u8> {
    let mut dirs: Vec<(&str, Vec<&str>)> = Vec::new();
    for f in files {
        let (dir, name) = f.rsplit_once('/').unwrap();
        match dirs.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, names)) => names.push(name),
            None => dirs.push((dir, vec![name])),
        }
    }
    let mut b = b"BSA\0".to_vec();
    let dir_names: usize = dirs.iter().map(|(d, _)| d.len() + 1).sum();
    let file_names: usize = files.iter().map(|f| f.rsplit_once('/').unwrap().1.len() + 1).sum();
    for v in [105, 36, 3, dirs.len(), files.len(), dir_names, file_names, 0] {
        b.extend((v as u32).to_le_bytes());
    }
    for (_, names) in &dirs {
        b.extend([0u8; 8]);
        b.extend((names.len() as u32).to_le_bytes());
        b.extend([0u8; 12]);
    }
    for (dir, names) in &dirs {
        b.push(dir.len() as u8 + 1);
        b.extend(dir.replace('/', "\\").as_bytes());
        b.push(0);
        b.extend(vec![0u8; 16 * names.len()]);
    }
    for (_, names) in &dirs {
        for n in names {
            b.extend(n.as_bytes());
            b.push(0);
        }
    }
    b
}

/// Archive conflicts follow plugin load order, and loose files beat every archive.
#[test]
fn archive_conflicts() {
    use rmm_core::ops;
    let tmp = tempfile::tempdir().unwrap();
    let (game, local, src, root) = (tmp.path().join("game"), tmp.path().join("local"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&src.join("C/meshes/m.nif"), "loose");
    write(&src.join("A/A.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&src.join("A/A.bsa"), bsa_bytes(&["textures/x.dds", "textures/a.dds", "meshes/m.nif"]));
    write(&src.join("B/B.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&src.join("B/B - Textures.bsa"), bsa_bytes(&["textures/x.dds"]));
    write(&src.join("D/D.bsa"), bsa_bytes(&["textures/d.dds"]));
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &local).unwrap();
    for name in ["C", "A", "B", "D"] {
        install::install(&inst, &src.join(name), None, true, None).unwrap();
    }
    let get = |inst: &Instance, id: &str| conflicts::conflicts(inst, &inst.profile().unwrap()).unwrap().into_iter().find(|c| c.id == id).unwrap();

    let a = get(&inst, "A");
    assert_eq!((a.archived_files, a.archived_losing), (3, 2), "{a:?}");
    assert_eq!(a.archive_overwritten_by, ["C", "B"], "C's loose file beats A's archive despite its lower priority");
    assert_eq!(get(&inst, "B").archive_overwrites, ["A"]);
    assert_eq!(get(&inst, "C").archive_overwrites, ["A"]);
    let issues = health::check(&inst).unwrap();
    assert!(issues.iter().any(|i| i.kind == "archive_not_loaded" && i.subject == "D.bsa"), "{issues:?}");
    assert!(!issues.iter().any(|i| i.kind == "archive_not_loaded" && i.subject != "D.bsa"), "{issues:?}");

    ops::set_plugin_order(&inst, &["B.esp".into(), "A.esp".into()]).unwrap();
    let a = get(&inst, "A");
    assert_eq!((a.archived_losing, a.archive_overwritten_by.as_slice()), (1, ["C".to_string()].as_slice()));
    assert_eq!(a.archive_overwrites, ["B"]);
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in bytes {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// LOOT sorting and messages from a cached masterlist (no network: the cache is fresh).
#[test]
fn loot_sorts_and_reports() {
    use rmm_core::loot;
    let tmp = tempfile::tempdir().unwrap();
    let (game, local, src, root) = (tmp.path().join("game"), tmp.path().join("local"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let dirty = plugin_bytes(&["Skyrim.esm"], false);
    for name in ["First", "Second"] {
        write(&src.join(name).join(format!("{name}.esp")), plugin_bytes(&["Skyrim.esm"], false));
    }
    write(&src.join("Dirty/Dirty.esp"), &dirty);
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &local).unwrap();
    for name in ["First", "Second", "Dirty"] {
        install::install(&inst, &src.join(name), None, true, None).unwrap();
    }
    write(
        &root.join("loot/masterlist.yaml"),
        format!(
            "plugins:\n  - name: 'First.esp'\n    after: ['Second.esp']\n    msg:\n      - type: warn\n        content: 'First needs a patch'\n  \
             - name: 'Dirty.esp'\n    dirty:\n      - crc: 0x{:08X}\n        util: 'SSEEdit v4.1.5'\n        itm: 3\n",
            crc32(&dirty)
        ),
    );
    write(&root.join("loot/prelude.yaml"), "common: []\n");
    deploy::deploy(&inst, false).unwrap();

    let plan = loot::sort(&inst, true).unwrap();
    let pos = |order: &[String], n: &str| order.iter().position(|x| x == n).unwrap();
    assert!(pos(&plan.order, "Second.esp") < pos(&plan.order, "First.esp"), "{plan:?}");
    assert!(!plan.moved.is_empty() && plan.warnings.is_empty(), "{plan:?}");
    let names = |inst: &Instance| inst.profile().unwrap().plugins.into_iter().map(|p| p.name).collect::<Vec<_>>();
    assert_eq!(names(&inst), ["First.esp", "Second.esp", "Dirty.esp"], "a dry run changes nothing");

    loot::sort(&inst, false).unwrap();
    let sorted = names(&inst);
    assert!(sorted.iter().position(|n| n == "Second.esp") < sorted.iter().position(|n| n == "First.esp"));
    assert!(!sorted.iter().any(|n| n == "Skyrim.esm"), "base masters stay out of the profile");
    assert!(loot::sort(&inst, true).unwrap().moved.is_empty(), "sorting is stable");
    inst.undo().unwrap();
    assert_eq!(names(&inst), ["First.esp", "Second.esp", "Dirty.esp"]);

    let issues = health::check(&inst).unwrap();
    assert!(issues.iter().any(|i| i.kind == "loot_message" && i.subject == "First.esp" && i.message.contains("patch")), "{issues:?}");
    assert!(issues.iter().any(|i| i.kind == "dirty_plugin" && i.subject == "Dirty.esp" && i.message.starts_with("3 ITMs")), "{issues:?}");
}

/// A fake tool (a .bat) exercising every capture path: directed output, a new Data file, an
/// in-place edit of a swapped link, a rewrite of a vanilla file kept for restore, and an
/// in-place write through a link that isn't in the tool's rewrites.
#[cfg(windows)]
#[test]
fn tool_runs_capture_output_into_a_generated_mod() {
    use rmm_core::tools::{self, ToolSpec};
    use std::collections::BTreeMap;
    let tmp = tempfile::tempdir().unwrap();
    let (game, local, src, root) = (tmp.path().join("game"), tmp.path().join("local"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&game.join("Data/vanilla.ini"), "vanilla");
    write(&src.join("BodySlide/CalienteTools/BodySlide/Config.xml"), "orig-config");
    write(&src.join("BodySlide/CalienteTools/BodySlide/Other.xml"), "other");
    write(&src.join("Plain/meshes/a.nif"), "a-mesh");
    write(
        &src.join("tool.bat"),
        [
            r"@echo off",
            r#"mkdir "%~1\meshes""#,
            r#"echo built> "%~1\meshes\body.nif""#,
            r#"echo setting>> "%~2\CalienteTools\BodySlide\Config.xml""#,
            r#"echo new> "%~2\newfile.txt""#,
            r#"echo cleaned> "%~2\vanilla.ini""#,
            r#"echo oops>> "%~2\meshes\a.nif""#,
            r"echo done",
        ]
        .join("\r\n"),
    );

    let inst = Instance::create(&root, Game::SkyrimSe, &game, &local).unwrap();
    install::install(&inst, &src.join("BodySlide"), None, true, None).unwrap();
    install::install(&inst, &src.join("Plain"), None, true, None).unwrap();
    deploy::deploy(&inst, false).unwrap();
    let spec = ToolSpec {
        exe: src.join("tool.bat"),
        args: Some(vec!["{output}".into(), "{data}".into()]),
        rewrites: Some(vec!["vanilla.ini".into(), "CalienteTools/BodySlide/*.xml".into()]),
        ..Default::default()
    };
    tools::add(&inst, "gen", spec).unwrap();

    let plan = tools::run(&inst, "gen", &BTreeMap::new(), true, None).unwrap();
    assert_eq!(plan.swapped.len(), 2, "{plan:?}");
    assert!(!game.join("Data/newfile.txt").exists(), "a dry run changes nothing");

    let r = tools::run(&inst, "gen", &BTreeMap::new(), false, None).unwrap();
    assert_eq!(r.mod_id.as_deref(), Some("gen Output"), "{r:?}");
    assert_eq!(r.exit_code, Some(0));
    assert!(r.log_tail.contains("done"), "{r:?}");
    let out = root.join("mods/gen Output");
    assert!(read(&out.join("meshes/body.nif")).contains("built"));
    assert!(read(&out.join("newfile.txt")).contains("new"));
    assert!(read(&out.join("vanilla.ini")).contains("cleaned"));
    assert!(read(&out.join("CalienteTools/BodySlide/Config.xml")).contains("setting"));
    assert!(read(&out.join("meshes/a.nif")).contains("oops"));
    // The swapped link kept the mod store clean; the unlisted one wrote through and is reported.
    assert_eq!(read(&root.join("mods/BodySlide/CalienteTools/BodySlide/Config.xml")), "orig-config");
    assert_eq!(r.modified_in_place, vec!["Plain: meshes/a.nif".to_string()]);
    // Untouched swapped copies are links again; the vanilla original went to backup/.
    assert!(same_file::is_same_file(game.join("Data/CalienteTools/BodySlide/Other.xml"), root.join("mods/BodySlide/CalienteTools/BodySlide/Other.xml")).unwrap());
    assert!(same_file::is_same_file(game.join("Data/newfile.txt"), out.join("newfile.txt")).unwrap());
    assert_eq!(read(&root.join("backup/Data/vanilla.ini")), "vanilla");
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);

    // A second run replaces the output; undo brings the first one back.
    let r2 = tools::run(&inst, "gen", &BTreeMap::new(), false, None).unwrap();
    assert_eq!(r2.previous.as_deref(), Some("gen Output"));
    assert_eq!(r2.mod_id.as_deref(), Some("gen Output (2)"));
    let ids: Vec<String> = inst.profile().unwrap().mods.into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["BodySlide", "Plain", "gen Output (2)"]);
    assert!(root.join("trash/gen Output").is_dir());
    inst.undo().unwrap();
    assert!(inst.profile().unwrap().mods.iter().any(|m| m.id == "gen Output"));
    assert!(out.is_dir());

    deploy::purge(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/vanilla.ini")), "vanilla");
    assert!(!game.join("Data/newfile.txt").exists() && !game.join("Data/meshes/body.nif").exists());
}
