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
    let txt = read(&local.join("Plugins.txt")); // libloadorder's spelling; Linux is case-sensitive
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

    // User rules go to the userlist and steer the next sort; an empty rule removes one.
    let rule = loot::Rule { plugin: "Second.esp".into(), after: vec!["Dirty.esp".into()], group: None };
    assert_eq!(loot::set_rule(&inst, &rule).unwrap(), None);
    assert_eq!(loot::rules(&inst).unwrap(), [rule]);
    assert!(read(&root.join("loot/userlist.yaml")).contains("Dirty.esp"));
    let plan = loot::sort(&inst, true).unwrap();
    assert!(pos(&plan.order, "Dirty.esp") < pos(&plan.order, "Second.esp"), "{plan:?}");
    let bad = loot::Rule { plugin: "First.esp".into(), group: Some("Nope".into()), ..Default::default() };
    assert!(loot::set_rule(&inst, &bad).unwrap_err().to_string().contains("no group"));
    let previous = loot::set_rule(&inst, &loot::Rule { plugin: "Second.esp".into(), ..Default::default() }).unwrap();
    assert_eq!(previous.unwrap().after, ["Dirty.esp"]);
    assert!(loot::rules(&inst).unwrap().is_empty());
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
            r#"echo enb> "%~3\enblocal.ini""#,
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
        args: Some(vec!["{output}".into(), "{data}".into(), "{game}".into()]),
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
    // Files written to the game root go to the mod's root/ and are deployed back there.
    assert!(read(&out.join("root/enblocal.ini")).contains("enb"));
    assert!(same_file::is_same_file(game.join("enblocal.ini"), out.join("root/enblocal.ini")).unwrap());
    // The swapped link kept the mod store clean; the unlisted one wrote through and is reported.
    assert_eq!(read(&root.join("mods/BodySlide/CalienteTools/BodySlide/Config.xml")), "orig-config");
    assert_eq!(r.modified_in_place, vec!["Plain: meshes/a.nif".to_string()]);
    // Untouched swapped copies are links again; the vanilla original went to backup/.
    assert!(same_file::is_same_file(game.join("Data/CalienteTools/BodySlide/Other.xml"), root.join("mods/BodySlide/CalienteTools/BodySlide/Other.xml")).unwrap());
    assert!(same_file::is_same_file(game.join("Data/newfile.txt"), out.join("newfile.txt")).unwrap());
    assert_eq!(read(&root.join("backup/Data/vanilla.ini")), "vanilla");
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);

    // A second run replaces the output; undo brings the first one back. Mod rules follow the new id.
    use rmm_core::rules::{self, ModRule};
    rules::set_rule(&inst, &ModRule { id: "gen Output".into(), after: vec!["Plain".into()] }).unwrap();
    let r2 = tools::run(&inst, "gen", &BTreeMap::new(), false, None).unwrap();
    assert_eq!(r2.previous.as_deref(), Some("gen Output"));
    assert_eq!(r2.mod_id.as_deref(), Some("gen Output (2)"));
    assert_eq!(rules::rules(&inst).unwrap()[0].id, "gen Output (2)");
    let ids: Vec<String> = inst.profile().unwrap().mods.into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["BodySlide", "Plain", "gen Output (2)"]);
    assert!(root.join("trash/gen Output").is_dir());
    inst.undo().unwrap();
    assert!(inst.profile().unwrap().mods.iter().any(|m| m.id == "gen Output"));
    assert!(out.is_dir());

    deploy::purge(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/vanilla.ini")), "vanilla");
    assert!(!game.join("Data/newfile.txt").exists() && !game.join("Data/meshes/body.nif").exists());
    assert!(!game.join("enblocal.ini").exists());
}

/// A JSON settings file is merged before the run (other keys and a BOM survive), kept files carry
/// over from the previous output, and a run that adds a plugin is sorted with the cached masterlist.
#[cfg(windows)]
#[test]
fn tool_settings_kept_files_and_sort() {
    use rmm_core::tools::{self, Settings, ToolSpec};
    use std::collections::BTreeMap;
    let tmp = tempfile::tempdir().unwrap();
    let (game, local, src, root) = (tmp.path().join("game"), tmp.path().join("local"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&src.join("New.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&src.join("Settings.json"), "\u{feff}{\"app\":{\"theme\":1},\"games\":{\"SkyrimSE\":{\"outputPath\":\"old\"}}}");
    write(
        &src.join("tool.bat"),
        // The tool keeps a copy of the settings it saw (it runs in its own folder).
        [r"@echo off", r#"mkdir "%~1\Pandora_Engine" 2>nul"#, r#"echo run>> "%~1\Pandora_Engine\ActiveMods.json""#, r#"copy /y "%~2" "%~1\New.esp" >nul"#, r#"copy /y Settings.json "%~1\seen.json" >nul"#].join("\r\n"),
    );
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &local).unwrap();
    write(&root.join("loot/masterlist.yaml"), "plugins: []\n");
    let set = BTreeMap::from([("games.SkyrimSE.gameDataPath".to_string(), "{data}".into()), ("games.SkyrimSE.outputPath".to_string(), "{output}".into())]);
    let spec = ToolSpec {
        exe: src.join("tool.bat"),
        args: Some(vec!["{output}".into(), "{esp}".into()]),
        vars: BTreeMap::from([("esp".to_string(), src.join("New.esp").display().to_string())]),
        keep: Some(vec!["Pandora_Engine/*.json".into()]),
        settings: Some(Settings { file: "Settings.json".into(), set }),
        ..Default::default()
    };
    tools::add(&inst, "gen", spec).unwrap();

    let r = tools::run(&inst, "gen", &BTreeMap::new(), false, None).unwrap();
    assert_eq!(r.mod_id.as_deref(), Some("gen Output"), "{r:?}");
    let seen: serde_json::Value = serde_json::from_str(&read(&root.join("mods/gen Output/seen.json"))).unwrap();
    assert_eq!(seen["app"]["theme"], 1);
    assert_eq!(seen["games"]["SkyrimSE"]["gameDataPath"].as_str().unwrap().replace('/', "\\"), inst.data_dir().display().to_string().replace('/', "\\"));
    assert!(seen["games"]["SkyrimSE"]["outputPath"].as_str().unwrap().ends_with("tool-gen"));
    // After the run, rmm's keys are back as they were: outputPath restored, gameDataPath gone.
    let after: serde_json::Value = serde_json::from_str(&read(&src.join("Settings.json"))).unwrap();
    assert_eq!(after, serde_json::json!({"app": {"theme": 1}, "games": {"SkyrimSE": {"outputPath": "old"}}}));
    assert!(r.sort.is_some(), "{r:?}");
    assert!(inst.profile().unwrap().plugins.iter().any(|p| p.name == "New.esp" && p.enabled));

    // The kept file is carried into the next run, which appends to it; no new plugin, no sort.
    let r2 = tools::run(&inst, "gen", &BTreeMap::new(), false, None).unwrap();
    assert_eq!(read(&root.join("mods/gen Output (2)/Pandora_Engine/ActiveMods.json")).matches("run").count(), 2);
    assert!(r2.sort.is_none() && r2.warnings.is_empty(), "{r2:?}");
}

/// Launch reads the game's version resource, refuses a script extender built for another version,
/// and starts the matching loader from the game folder. PING.EXE stands in for game and loader.
#[cfg(windows)]
#[test]
fn launch_checks_the_script_extender() {
    use rmm_core::launch;
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    fs::copy(r"C:\Windows\System32\PING.EXE", game.join("SkyrimSE.exe")).unwrap();
    let plain = launch::launch(&inst, true).unwrap();
    assert_eq!(plain.exe, game.join("SkyrimSE.exe"));
    assert!(plain.pid.is_none() && plain.script_extender.is_none(), "{plain:?}");

    fs::copy(r"C:\Windows\System32\PING.EXE", game.join("skse64_loader.exe")).unwrap();
    write(&game.join("skse64_1_5_97.dll"), "");
    let err = launch::launch(&inst, true).unwrap_err().to_string();
    assert!(err.contains("1.5.97") && err.contains(&plain.game_version), "{err}");

    let v: Vec<&str> = plain.game_version.split('.').collect();
    let dll = format!("skse64_{}_{}_{}.dll", v[0], v[1], v[2]);
    write(&game.join(&dll), "");
    let r = launch::launch(&inst, false).unwrap();
    assert_eq!(r.script_extender.as_deref(), Some(dll.as_str()));
    assert!(r.exe.ends_with("skse64_loader.exe") && r.pid.is_some(), "{r:?}");
}

/// Wait until the stand-in game (a PING.EXE copy) has exited.
#[cfg(windows)]
fn wait_until_closed(exe: &Path) {
    for _ in 0..100 {
        if fs::OpenOptions::new().write(true).open(exe).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("{} still running", exe.display());
}

/// `wait_for`: the tool is a launcher that starts the "game" and exits at once; the run waits
/// for the game and captures what it leaves behind only after it closed.
#[cfg(windows)]
#[test]
fn tool_runs_wait_for_what_a_launcher_starts() {
    use rmm_core::tools::{self, ToolSpec};
    use std::collections::BTreeMap;
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    fs::copy(r"C:\Windows\System32\PING.EXE", game.join("SkyrimSE.exe")).unwrap();
    write(
        &tmp.path().join("loader.bat"),
        [r"@echo off", r#"start "" /b "%~1\SkyrimSE.exe" -n 3 127.0.0.1"#, r#"echo grass> "%~1\Data\grass.cgid""#].join("\r\n"),
    );
    let spec = ToolSpec { exe: tmp.path().join("loader.bat"), args: Some(vec!["{game}".into()]), wait_for: Some("SkyrimSE.exe".into()), ..Default::default() };
    tools::add(&inst, "precache", spec).unwrap();
    let start = std::time::Instant::now();
    let r = tools::run(&inst, "precache", &BTreeMap::new(), false, None).unwrap();
    assert!(start.elapsed() >= std::time::Duration::from_millis(1500), "returned before the game closed: {r:?}");
    assert!(r.warnings.is_empty(), "{r:?}");
    assert_eq!(r.new, ["Data/grass.cgid"]);
}

/// After `launch_game`, the next deploy moves files that appeared in Data into overwrite/.
/// Script-extender plugin configs are deployed as copies: a plugin's rewrite on launch becomes an
/// external change in overwrite/, the mod's file stays as installed, and a newer mod file is
/// copied again.
#[cfg(windows)]
#[test]
fn launch_captures_what_the_game_writes() {
    use rmm_core::launch;
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    let root = tmp.path().join("inst");
    write(&tmp.path().join("Logger/SKSE/Plugins/CrashLogger.ini"), "installed");
    install::install(&inst, &tmp.path().join("Logger"), None, true, None).unwrap();
    deploy::deploy(&inst, false).unwrap();
    let (ini, src) = (game.join("Data/SKSE/Plugins/CrashLogger.ini"), root.join("mods/Logger/SKSE/Plugins/CrashLogger.ini"));
    assert!(!same_file::is_same_file(&ini, &src).unwrap(), "configs are copies");
    write(&src, "updated by the user");
    assert!(deploy::deploy(&inst, false).unwrap().link.iter().any(|l| l.ends_with("CrashLogger.ini")));
    assert_eq!(read(&ini), "updated by the user");

    write(&game.join("Data/old.txt"), "there before");
    fs::copy(r"C:\Windows\System32\PING.EXE", game.join("SkyrimSE.exe")).unwrap();
    launch::launch(&inst, false).unwrap();
    wait_until_closed(&game.join("SkyrimSE.exe"));
    // What the game and its plugins do during play.
    write(&game.join("Data/MCM/Settings/mod.ini"), "user setting");
    write(&ini, "rewritten on launch");

    let plan = deploy::deploy(&inst, true).unwrap();
    assert_eq!(plan.played, ["Data/MCM/Settings/mod.ini"]);
    assert_eq!(plan.external_changes, ["Data/SKSE/Plugins/CrashLogger.ini"]);
    deploy::deploy(&inst, false).unwrap();
    assert!(same_file::is_same_file(game.join("Data/MCM/Settings/mod.ini"), root.join("overwrite/MCM/Settings/mod.ini")).unwrap());
    assert_eq!(read(&root.join("overwrite/SKSE/Plugins/CrashLogger.ini")), "rewritten on launch");
    assert_eq!(read(&src), "updated by the user");
    assert!(deploy::deploy(&inst, true).unwrap().is_noop());

    deploy::purge(&inst, false).unwrap();
    assert!(!game.join("Data/MCM").exists() && !ini.exists());
    assert_eq!(read(&game.join("Data/old.txt")), "there before");
}

/// One case for `vortex_planner_rows`: mods V1 (`meshes/sky.nif` = "v1"), V2 ("v2") and Same
/// ("v1" again), with only V1 enabled, and nothing deployed yet.
fn planner_case(base: &Path) -> (Instance, std::path::PathBuf, std::path::PathBuf) {
    let (game, root) = (base.join("game"), base.join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &base.join("local")).unwrap();
    for (id, content, on) in [("V1", "v1", true), ("V2", "v2", false), ("Same", "v1", false)] {
        write(&base.join("src").join(id).join("meshes/sky.nif"), content);
        install::install(&inst, &base.join("src").join(id), None, on, None).unwrap();
    }
    (inst, game, root)
}

/// Vortex's next deploy planner (`packages/mod-pipeline/src/plan.ts`, `OPERATIONS_BY_STATE`, GPL-3.0)
/// decides per destination from (disk, previous deploy, current profile), with one JSON fixture
/// per row in `fixtures/planner/`. Each block sets up one row, named by Vortex's state code (disk,
/// previous, current: x = none, A/B/C = contents), and checks rmm's answer. rmm differs on
/// purpose in two ways: a file changed outside rmm goes to overwrite/ and keeps winning (MO2's
/// model) where Vortex backs it up and replaces it, and backups are restored on purge. The
/// "unhashed" and symlink rows don't apply: rmm compares file identity, not hashes.
#[test]
fn vortex_planner_rows() {
    use rmm_core::ops::set_mod_enabled;
    let tmp = tempfile::tempdir().unwrap();
    let t = "Data/meshes/sky.nif";
    let case = |name: &str| planner_case(&tmp.path().join(name));
    let on = |inst: &Instance, id: &str, e: bool| {
        set_mod_enabled(inst, id, e).unwrap();
    };
    let linked = |game: &Path, root: &Path, m: &str| same_file::is_same_file(game.join(t), root.join("mods").join(m).join("meshes/sky.nif")).unwrap();
    let replace = |game: &Path, s: &str| {
        fs::remove_file(game.join(t)).unwrap();
        write(&game.join(t), s);
    };
    let dry = |inst: &Instance| deploy::deploy(inst, true).unwrap();
    let apply = |inst: &Instance| deploy::deploy(inst, false).unwrap();

    // xxA new file: linked.
    let (inst, game, root) = case("new");
    assert_eq!(apply(&inst).link, [t]);
    assert!(linked(&game, &root, "V1"));

    // AxA, BxA: a file rmm didn't deploy is backed up, even with the same content, then linked.
    // Removing the mod restores it, and so does purge (Vortex keeps backups forever).
    for (name, there) in [("new-over-identical", "v1"), ("new-over-other", "user")] {
        let (inst, game, root) = case(name);
        write(&game.join(t), there);
        let plan = apply(&inst);
        assert!(plan.backup == [t] && plan.link == [t], "{name}: {plan:?}");
        assert!(linked(&game, &root, "V1") && read(&root.join("backup").join(t)) == there);
        on(&inst, "V1", false);
        apply(&inst);
        assert_eq!(read(&game.join(t)), there, "{name}: removing the mod restores the original");
        on(&inst, "V1", true);
        apply(&inst);
        deploy::purge(&inst, false).unwrap();
        assert_eq!(read(&game.join(t)), there, "{name}: purge restores the original");
    }

    // AAA unchanged, plus disk-only-file: files neither deploy knows are left alone.
    let (inst, game, _) = case("unchanged");
    apply(&inst);
    write(&game.join("Data/user-notes.txt"), "mine");
    assert!(dry(&inst).is_noop());
    deploy::purge(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/user-notes.txt")), "mine");

    // xAA deployed but missing: linked again.
    let (inst, game, root) = case("deployed-missing");
    apply(&inst);
    fs::remove_file(game.join(t)).unwrap();
    assert_eq!(apply(&inst).link, [t]);
    assert!(linked(&game, &root, "V1"));

    // BAA edited outside rmm: captured to overwrite/ and kept (Vortex: back up, relink).
    let (inst, game, root) = case("deployed-edited");
    apply(&inst);
    replace(&game, "edited");
    assert_eq!(dry(&inst).external_changes, [t]);
    apply(&inst);
    assert_eq!(read(&game.join(t)), "edited");
    assert_eq!(read(&root.join("overwrite/meshes/sky.nif")), "edited");
    assert_eq!(read(&root.join("mods/V1/meshes/sky.nif")), "v1");

    // xBA, BBA updated (another mod now wins), missing or still ours: linked to the new winner.
    // same-content-other-mod: the same bytes from another mod are relinked too.
    for (name, from, to, missing) in [("updated-missing", "V1", "V2", true), ("updated", "V1", "V2", false), ("same-content-other-mod", "V1", "Same", false)] {
        let (inst, game, root) = case(name);
        apply(&inst);
        if missing {
            fs::remove_file(game.join(t)).unwrap();
        }
        on(&inst, from, false);
        on(&inst, to, true);
        let plan = apply(&inst);
        assert!(plan.link == [t] && plan.external_changes.is_empty() && plan.backup.is_empty(), "{name}: {plan:?}");
        assert!(linked(&game, &root, to), "{name}");
    }

    // ABA, CBA: the old file was replaced outside rmm (with the update's bytes, or an edit) and
    // the profile now wants the update. The outside file goes to overwrite/ and wins.
    for (name, outside) in [("updated-over-identical", "v2"), ("updated-over-edited", "edited")] {
        let (inst, game, root) = case(name);
        apply(&inst);
        replace(&game, outside);
        on(&inst, "V2", true);
        assert_eq!(apply(&inst).external_changes, [t], "{name}");
        assert_eq!(read(&game.join(t)), outside, "{name}");
        assert_eq!(read(&root.join("mods/V2/meshes/sky.nif")), "v2", "{name}");
    }

    // xAx removed and already gone, AAx removed: nothing left behind.
    for (name, missing) in [("removed-gone", true), ("removed", false)] {
        let (inst, game, _) = case(name);
        apply(&inst);
        if missing {
            fs::remove_file(game.join(t)).unwrap();
        }
        on(&inst, "V1", false);
        apply(&inst);
        assert!(!game.join(t).exists() && dry(&inst).is_noop(), "{name}");
    }

    // BAx removed after an outside edit: the edit is kept in overwrite/ (Vortex: back up).
    let (inst, game, root) = case("removed-edited");
    apply(&inst);
    replace(&game, "edited");
    on(&inst, "V1", false);
    apply(&inst);
    assert_eq!(read(&game.join(t)), "edited");
    assert_eq!(read(&root.join("overwrite/meshes/sky.nif")), "edited");

    // deployed-file-other-case: the disk spells the folder differently; still the same file.
    #[cfg(windows)]
    {
        let (inst, game, _) = case("other-case");
        apply(&inst);
        fs::rename(game.join("Data/meshes"), game.join("Data/Meshes")).unwrap();
        assert!(dry(&inst).is_noop());
    }

    // file-replaces-folder, folder-replaces-file: the old entries are unlinked before the new
    // ones are linked, and purge leaves neither behind.
    let base = tmp.path().join("file-folder");
    let (inst, game, _) = planner_case(&base);
    write(&base.join("src/Dir/SkyUI/config.txt"), "config");
    write(&base.join("src/Dir/interface/dir.swf"), "swf"); // two top-level folders: not unwrapped
    write(&base.join("src/File/SkyUI"), "file");
    install::install(&inst, &base.join("src/Dir"), None, true, None).unwrap();
    install::install(&inst, &base.join("src/File"), None, false, None).unwrap();
    apply(&inst);
    on(&inst, "Dir", false);
    on(&inst, "File", true);
    let plan = apply(&inst);
    assert!(plan.backup.is_empty(), "rmm's own folder isn't someone else's file: {plan:?}");
    assert_eq!(read(&game.join("Data/SkyUI")), "file");
    on(&inst, "File", false);
    on(&inst, "Dir", true);
    apply(&inst);
    assert_eq!(read(&game.join("Data/SkyUI/config.txt")), "config");
    deploy::purge(&inst, false).unwrap();
    assert!(!game.join("Data/SkyUI").exists());
}

/// Linux: a Windows tool and the game run through the Proton build named in the prefix's
/// config_info, as `proton run <exe> <args>` with Steam's compat variables and Z: paths. The
/// Steam library and Proton are fakes: `proton` is a shell script that records its call and acts
/// like the Windows tool by writing into the folder it was given. Fallout 4, because on Linux a
/// running "game" is any process naming its exe, and the other tests deploy Skyrim in parallel.
#[cfg(unix)]
#[test]
fn proton_runs_windows_tools_and_the_game() {
    use rmm_core::{launch, tools::{self, ToolSpec}};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let game = base.join("lib/steamapps/common/Fallout 4");
    let compat = base.join("lib/steamapps/compatdata/377160");
    let mut exe = b"MZ".to_vec(); // a version resource for 1.10.163.0
    exe.extend([0xBD, 0x04, 0xEF, 0xFE, 0, 0, 1, 0]);
    exe.extend(((1u32 << 16) | 10).to_le_bytes());
    exe.extend((163u32 << 16).to_le_bytes());
    write(&game.join("Fallout4.exe"), exe);
    fs::create_dir_all(game.join("Data")).unwrap();
    let proton = base.join("Proton 9.0/files");
    write(
        &compat.join("config_info"),
        format!("9.0-201\n{p}/share/fonts/\n{p}/lib/\n{p}/lib64/\n{s}/\n", p = proton.display(), s = base.join("steam").display()),
    );
    let script = base.join("Proton 9.0/proton");
    write(
        &script,
        r#"#!/bin/sh
{ printf '%s\n' "$@"; env | grep -E '^(STEAM_COMPAT_|SteamAppId|SteamGameId)' | sort; } > "$STEAM_COMPAT_DATA_PATH/last-call"
shift 2
[ -n "$1" ] && out=$(printf '%s' "$1" | sed 's/^Z://; s#\\#/#g') && echo made > "$out/made.txt"
exit 0
"#,
    );
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let call = || fs::read_to_string(compat.join("last-call")).unwrap_or_default();

    let inst = Instance::create(&base.join("inst"), Game::Fallout4, &game, &base.join("local")).unwrap();
    write(&base.join("tools/Tool.exe"), "MZ");
    let spec = ToolSpec { exe: base.join("tools/Tool.exe"), args: Some(vec!["{output}".into()]), ..Default::default() };
    tools::add(&inst, "gen", spec).unwrap();
    let r = tools::run(&inst, "gen", &BTreeMap::new(), false, None).unwrap();
    assert_eq!(r.mod_id.as_deref(), Some("gen Output"), "{r:?}\n{}", call());
    assert_eq!(read(&base.join("inst/mods/gen Output/made.txt")).trim(), "made");
    let lines: Vec<String> = call().lines().map(str::to_owned).collect();
    assert_eq!(lines[0], "run");
    assert_eq!(lines[1], base.join("tools/Tool.exe").display().to_string());
    assert!(lines[2].starts_with(r"Z:\") && lines[2].ends_with(r"\.staging\tool-gen"), "{lines:?}");
    for want in [format!("STEAM_COMPAT_DATA_PATH={}", compat.display()), format!("STEAM_COMPAT_CLIENT_INSTALL_PATH={}", base.join("steam").display()), "SteamAppId=377160".into(), "SteamGameId=377160".into()] {
        assert!(lines.contains(&want), "missing {want}: {lines:?}");
    }

    // Plugin settings are deployed as copies, and an edited mod file is copied again: Linux
    // copies get the copy time as mtime, unlike Windows, where they keep the source's.
    write(&base.join("src/Logger/F4SE/Plugins/Logger.ini"), "installed");
    install::install(&inst, &base.join("src/Logger"), None, true, None).unwrap();
    deploy::deploy(&inst, false).unwrap();
    let (ini, src) = (game.join("Data/F4SE/Plugins/Logger.ini"), base.join("inst/mods/Logger/F4SE/Plugins/Logger.ini"));
    assert!(!same_file::is_same_file(&ini, &src).unwrap(), "configs are copies");
    assert!(deploy::deploy(&inst, true).unwrap().is_noop());
    write(&src, "updated");
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(read(&ini), "updated");

    // The game starts the same way, without waiting.
    fs::remove_file(compat.join("last-call")).unwrap();
    let r = launch::launch(&inst, false).unwrap();
    assert_eq!(r.game_version, "1.10.163.0");
    for _ in 0..50 {
        if call().lines().count() > 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let lines: Vec<String> = call().lines().map(str::to_owned).collect();
    assert_eq!(lines.get(..2), Some(&["run".to_string(), game.join("Fallout4.exe").display().to_string()][..]), "{lines:?}");

    // What the game writes during play is captured by the next deploy.
    write(&game.join("Data/MCM/Settings/mod.ini"), "user setting");
    write(&ini, "rewritten on launch");
    let plan = deploy::deploy(&inst, true).unwrap();
    assert_eq!((plan.played.as_slice(), plan.external_changes.as_slice()), (["Data/MCM/Settings/mod.ini".to_string()].as_slice(), ["Data/F4SE/Plugins/Logger.ini".to_string()].as_slice()));
    // The fake game may still be exiting; deploy refuses while it runs.
    let mut tries = 0;
    while let Err(e) = deploy::deploy(&inst, false) {
        assert!(e.to_string().contains("running") && tries < 50, "{e:#}");
        tries += 1;
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(read(&base.join("inst/overwrite/MCM/Settings/mod.ini")), "user setting");
    assert_eq!(read(&base.join("inst/overwrite/F4SE/Plugins/Logger.ini")), "rewritten on launch");
    assert_eq!(read(&src), "updated");
}

/// Profiles: each has its own enabled mods, order, plugins and undo history; switching brings
/// in mods installed elsewhere (disabled) and drops uninstalled ones; deploy follows the switch.
#[test]
fn profiles() {
    use rmm_core::ops::set_mod_enabled;
    let tmp = tempfile::tempdir().unwrap();
    let (game, local, src, root) = (tmp.path().join("game"), tmp.path().join("local"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    for id in ["A", "B", "C"] {
        write(&src.join(id).join(format!("{id}.esp")), plugin_bytes(&["Skyrim.esm"], false));
        write(&src.join(id).join("textures/sky.dds"), id);
    }
    let mut inst = Instance::create(&root, Game::SkyrimSe, &game, &local).unwrap();
    install::install(&inst, &src.join("A"), None, true, None).unwrap();
    install::install(&inst, &src.join("B"), None, true, None).unwrap();
    deploy::deploy(&inst, false).unwrap();
    let state = |inst: &Instance| {
        let p = inst.profile().unwrap();
        let on = |v: Vec<(String, bool)>| v.into_iter().map(|(n, e)| format!("{}{n}", if e { "+" } else { "-" })).collect::<Vec<_>>().join(" ");
        (on(p.mods.into_iter().map(|m| (m.id, m.enabled)).collect()), on(p.plugins.into_iter().map(|e| (e.name, e.enabled)).collect()))
    };

    // Names are folder names, unique without regard to case.
    for bad in ["", "../x", ".hidden", "a:b", " pad"] {
        assert!(inst.create_profile(bad, true).is_err(), "{bad:?}");
    }
    inst.create_profile("Lean", false).unwrap();
    inst.create_profile("Copy", true).unwrap();
    assert!(inst.create_profile("lean", true).is_err());
    assert_eq!(inst.profiles().unwrap(), ["Copy", "default", "Lean"]);

    // An empty profile gets every installed mod, disabled, on the first switch.
    inst.switch_profile("lean").unwrap();
    assert_eq!(inst.config.profile, "Lean");
    assert_eq!(Instance::open(&root).unwrap().config.profile, "Lean");
    assert_eq!(state(&inst), ("-A -B".into(), "-A.esp -B.esp".into()));
    assert!(inst.history().unwrap().is_empty(), "history is per profile");
    set_mod_enabled(&inst, "A", true).unwrap();
    let plan = deploy::deploy(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/textures/sky.dds")), "A");
    assert!(plan.link.contains(&"Data/textures/sky.dds".to_string()) && !game.join("Data/B.esp").exists(), "{plan:?}");
    let txt = read(&local.join("Plugins.txt"));
    assert!(txt.contains("*A.esp") && !txt.contains("B.esp"), "{txt}");

    // A mod installed here shows up in the others, disabled, after its plugins.
    install::install(&inst, &src.join("C"), None, true, None).unwrap();
    inst.switch_profile("default").unwrap();
    assert_eq!(state(&inst), ("+A +B -C".into(), "+A.esp +B.esp -C.esp".into()));
    assert_eq!(inst.history().unwrap(), ["install B", "install A"]);
    assert_eq!(deploy::deploy(&inst, true).unwrap().link.len(), 2, "B.esp and sky.dds come back");

    // A mod uninstalled here leaves the others on the next switch.
    install::uninstall(&inst, "B").unwrap();
    inst.switch_profile("Copy").unwrap();
    assert_eq!(state(&inst), ("+A -C".into(), "+A.esp +B.esp -C.esp".into()), "plugins keep their slot");

    // Rename (the active one too) and delete (never the active one).
    inst.rename_profile("Copy", "Full").unwrap();
    assert_eq!((inst.config.profile.as_str(), Instance::open(&root).unwrap().config.profile.as_str()), ("Full", "Full"));
    assert!(inst.rename_profile("Lean", "default").is_err());
    assert!(inst.delete_profile("full").is_err());
    inst.delete_profile("Lean").unwrap();
    assert_eq!(inst.profiles().unwrap(), ["default", "Full"]);
    assert!(root.join("profiles/.deleted/Lean/profile.toml").is_file());
}

/// Mod rules: suggestions the health check reports and sort_mods applies, never a block on
/// manual ordering.
#[test]
fn mod_rules() {
    use rmm_core::ops::{move_mod, set_mod_enabled};
    use rmm_core::rules::{self, ModRule};
    let tmp = tempfile::tempdir().unwrap();
    let (game, src, root) = (tmp.path().join("game"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    for id in ["A", "B", "C"] {
        write(&src.join(id).join("textures/sky.dds"), id);
        install::install(&inst, &src.join(id), None, true, None).unwrap();
    }
    let rule = |id: &str, after: &[&str]| ModRule { id: id.into(), after: after.iter().map(|s| s.to_string()).collect() };
    let ids = |inst: &Instance| inst.profile().unwrap().mods.into_iter().map(|m| m.id).collect::<Vec<_>>().join(" ");
    let broken = |inst: &Instance| health::check(inst).unwrap().into_iter().filter(|i| i.kind == "mod_rule_broken").map(|i| format!("{}: {}", i.subject, i.message)).collect::<Vec<_>>();

    for bad in [rule("A", &["A"]), rule("A", &["Nope"]), rule("Nope", &["A"]), rule("A", &["<overwrite>"])] {
        assert!(rules::set_rule(&inst, &bad).is_err(), "{bad:?}");
    }
    assert_eq!(rules::set_rule(&inst, &rule("A", &["C", "C"])).unwrap(), None);
    assert_eq!(rules::rules(&inst).unwrap(), [rule("A", &["C"])]);
    assert_eq!(broken(&inst), ["A: should load after C (a mod rule); sort_mods fixes the order"]);

    // Manual ordering still works; a rule between a disabled mod and others isn't reported.
    move_mod(&inst, "C", 2).unwrap();
    set_mod_enabled(&inst, "C", false).unwrap();
    assert!(broken(&inst).is_empty());
    set_mod_enabled(&inst, "C", true).unwrap();

    // The sort moves only what the rule forces, and is undoable.
    let plan = rules::sort(&inst, true).unwrap();
    assert_eq!((plan.order.join(" "), ids(&inst)), ("B C A".into(), "A B C".into()));
    rules::sort(&inst, false).unwrap();
    assert_eq!(ids(&inst), "B C A");
    assert!(broken(&inst).is_empty());
    assert_eq!(inst.history().unwrap()[0], "sort mods by rules");

    // A cycle is reported and one of its rules left out; the others still apply.
    assert_eq!(rules::set_rule(&inst, &rule("C", &["A"])).unwrap(), None);
    let plan = rules::sort(&inst, true).unwrap();
    assert_eq!(plan.cycles.len(), 1, "{plan:?}");
    // An empty rule removes it and returns it.
    assert_eq!(rules::set_rule(&inst, &rule("C", &[])).unwrap(), Some(rule("C", &["A"])));
    assert_eq!(rules::rules(&inst).unwrap(), [rule("A", &["C"])]);
}
