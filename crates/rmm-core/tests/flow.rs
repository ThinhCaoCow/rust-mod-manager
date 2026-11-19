//! End-to-end check on a synthetic Skyrim SE folder: install -> conflicts -> deploy -> reorder
//! -> external change -> undo/redo -> uninstall/undo -> purge.
use rmm_core::{Game, Instance, conflicts, deploy, health, install, ops, plugins};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Minimal valid Skyrim SE plugin: a TES4 record with HEDR and MAST/DATA pairs.
/// Settings and keys from an empty folder, never the user's: their Settings tab changes what rmm
/// reports (hidden problem kinds, LOOT's notes, deploy method).
fn isolate() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // Before any test reads the environment: each one calls this first. A test that sets its own
    // folder keeps it.
    ONCE.call_once(|| {
        if std::env::var_os("RMM_CONFIG_DIR").is_none() {
            unsafe { std::env::set_var("RMM_CONFIG_DIR", std::env::temp_dir().join(format!("rmm-test-config-{}", std::process::id()))) };
        }
    });
}

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
    isolate();
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

    // Mod B: zip with one wrapper folder (the installer must descend into it), named .7z as
    // some uploads are: the installer goes by its first bytes.
    let zip_path = src.join("B-1-0.7z");
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
    assert_eq!(rmm_core::launch::script_extender_mods(&inst).unwrap(), std::slice::from_ref(&s.id));

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
    isolate();
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

    // overwrite/ becomes a mod of its own; the deployed files are already its files.
    let r = install::mod_from_overwrite(&inst, "Captured").unwrap();
    assert_eq!(r.files, 2);
    assert!(inst.mod_files(rmm_core::instance::OVERWRITE).unwrap().is_empty());
    assert_eq!(inst.profile().unwrap().mods.last().unwrap().id, r.id);
    let plan = deploy::deploy(&inst, false).unwrap();
    assert!(plan.external_changes.is_empty() && plan.remove.is_empty(), "{plan:?}");
    assert_eq!(read(&game.join("Data/textures/sky.dds")), "Z");
    assert!(install::mod_from_overwrite(&inst, "Again").is_err());
    inst.undo().unwrap();
    // Clearing moves what's there to trash/ as a mod; the next deploy removes it from Data.
    write(&inst.root.join("overwrite/meshes/stray.nif"), "stray");
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(install::clear_overwrite(&inst).unwrap(), 1);
    assert_eq!(install::clear_overwrite(&inst).unwrap(), 0);
    let plan = deploy::deploy(&inst, false).unwrap();
    assert!(plan.external_changes.is_empty(), "{plan:?}");
    assert!(!game.join("Data/meshes/stray.nif").exists());
    assert!(inst.root.join("trash/Cleared overwrite/meshes/stray.nif").is_file());
    // Undo brings the files back, as a mod at the end of the list; redo clears them again.
    assert_eq!(inst.undo().unwrap().as_deref(), Some("clear overwrite"));
    assert_eq!(inst.profile().unwrap().mods.last().unwrap().id, "Cleared overwrite");
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/meshes/stray.nif")), "stray");
    inst.redo().unwrap();
    deploy::deploy(&inst, false).unwrap();
    assert!(!game.join("Data/meshes/stray.nif").exists() && inst.mod_files(rmm_core::instance::OVERWRITE).unwrap().is_empty());
}

#[test]
fn updates_a_mod_in_place() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write(&tmp.path().join("game/SkyrimSE.exe"), "exe");
    write(&tmp.path().join("game/Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&src.join("Snow Plus-4567-1-0-1700000000/Snow.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&src.join("Snow Plus-4567-1-0-1700000000/textures/snow.dds"), "v1");
    write(&src.join("Snow Plus-4567-2-0-1710000000/Snow.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&src.join("Snow Plus-4567-2-0-1710000000/SnowExtra.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&src.join("Snow Plus-4567-2-0-1710000000/textures/snow.dds"), "v2");
    write(&src.join("Other/textures/o.dds"), "o");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &tmp.path().join("game"), &tmp.path().join("local")).unwrap();
    let (v1, v2) = (src.join("Snow Plus-4567-1-0-1700000000"), src.join("Snow Plus-4567-2-0-1710000000"));
    let snow = install::install(&inst, &v1, Some("Snow Plus"), true, None).unwrap().id;
    install::install(&inst, &src.join("Other"), None, true, None).unwrap();
    ops::move_mods(&inst, &["Other".into()], 0).unwrap();
    ops::set_mod_note(&inst, &snow, "keep the 2K textures").unwrap();
    ops::set_color(&inst, &snow, "#336699").unwrap();
    let order = inst.profile().unwrap().mods;

    // The new download is recognised as the same Nexus mod, by its file name.
    assert_eq!(install::same_mod(&inst, &v2, None).unwrap(), Some((snow.clone(), "Snow Plus".to_string())));
    assert_eq!(install::same_mod(&inst, &src.join("Other"), None).unwrap(), None);
    assert_eq!(install::same_mod(&inst, &src.join("Other"), Some(4567)).unwrap().map(|m| m.0), Some(snow.clone()), "a Nexus download names its mod");

    let r = install::update(&inst, &snow, &v2, None).unwrap();
    assert_eq!((r.id.as_str(), r.name.as_str(), r.plugins.len()), (snow.as_str(), "Snow Plus", 2));
    let p = inst.profile().unwrap();
    assert_eq!((p.mods, p.colors.get(&snow).map(String::as_str)), (order.clone(), Some("#336699")));
    assert!(p.plugins.iter().any(|e| e.name == "SnowExtra.esp" && e.enabled), "the new plugin comes enabled like the mod");
    let meta = inst.mod_meta(&snow);
    assert_eq!(meta.notes.as_deref(), Some("keep the 2K textures"));
    assert!(meta.source.unwrap().ends_with("Snow Plus-4567-2-0-1710000000"));
    assert_eq!(read(&inst.mod_dir(&snow).join("textures/snow.dds")), "v2");
    assert_eq!(inst.history().unwrap()[0], format!("update {snow}"));

    // Undo swaps the old files back; redo brings the update again. A note written in between
    // stays, and the source goes back with the files.
    ops::set_mod_note(&inst, &snow, "v2 is better").unwrap();
    inst.undo().unwrap();
    assert_eq!(read(&inst.mod_dir(&snow).join("textures/snow.dds")), "v1");
    let meta = inst.mod_meta(&snow);
    assert_eq!(meta.notes.as_deref(), Some("v2 is better"));
    assert!(meta.source.unwrap().ends_with("Snow Plus-4567-1-0-1700000000"));
    assert!(!inst.mod_dir(&snow).join("SnowExtra.esp").exists());
    assert_eq!(inst.profile().unwrap().mods, order);
    inst.redo().unwrap();
    assert_eq!(read(&inst.mod_dir(&snow).join("textures/snow.dds")), "v2");
    // Once the trash is emptied, undo can't swap: the new files stay.
    install::empty_trash(&inst).unwrap();
    inst.undo().unwrap();
    assert_eq!(read(&inst.mod_dir(&snow).join("textures/snow.dds")), "v2");
    assert!(install::update(&inst, "sep:Nope", &v2, None).is_err());
    assert!(install::update(&inst, &snow, &src.join("Gone"), None).unwrap_err().to_string().contains("is gone"));
}

/// A game update: Problems says the version changed since the last Play, that Address Library has
/// no file for it, and which script extender plugins didn't load in the last run.
#[test]
fn guards_against_game_updates() {
    isolate();
    use rmm_core::{launch, logs};
    use std::time::{Duration, SystemTime};
    let tmp = tempfile::tempdir().unwrap();
    let (game, my) = (tmp.path().join("game"), tmp.path().join("my"));
    let exe = |pad: &[u8], build: u32| {
        let mut e = [b"MZ", pad].concat();
        e.extend([0xBD, 0x04, 0xEF, 0xFE, 0, 0, 1, 0]);
        e.extend(((1u32 << 16) | 6).to_le_bytes());
        e.extend((build << 16).to_le_bytes());
        e
    };
    write(&game.join("SkyrimSE.exe"), exe(b"", 1170));
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let mut inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    inst.config.ini_path = Some(my.clone());
    let src = tmp.path().join("src");
    write(&src.join("Address Library/SKSE/Plugins/versionlib-1-6-1170-0.bin"), "lib");
    write(&src.join("Fixes/SKSE/Plugins/Fixes.dll"), "dll");
    for m in ["Address Library", "Fixes"] {
        install::install(&inst, &src.join(m), None, true, None).unwrap();
    }
    deploy::deploy(&inst, false).unwrap();
    let ours = ["game_updated", "address_library_mismatch", "script_extender_plugin"];
    let found = |inst: &Instance| -> Vec<(String, String)> {
        health::check(inst).unwrap().into_iter().filter(|i| ours.contains(&i.kind)).map(|i| (i.kind.to_string(), i.subject)).collect()
    };
    assert_eq!(found(&inst), []);

    // The last Play was on 1.6.1170; then the game updates to 1.6.1179.
    fs::write(inst.root.join("game-version.txt"), "1.6.1170.0").unwrap();
    assert_eq!(found(&inst), []);
    write(&game.join("SkyrimSE.exe"), exe(b"!", 1179));
    assert_eq!(found(&inst), [("game_updated".into(), "SkyrimSE.exe".into()), ("address_library_mismatch".into(), "Address Library".into())]);
    let w = launch::launch(&inst, true).unwrap().warnings.join("\n");
    assert!(w.contains("the game was 1.6.1170.0 at your last Play and is 1.6.1179.0 now"), "{w}");
    assert!(w.contains("Address Library has no file for game version 1.6.1179.0 (its newest are for 1.6.1170.0)"), "{w}");

    // The last run's log: Fixes.dll (from a mod) didn't load; a DLL no mod has is left out.
    let log = my.join("SKSE/skse64.log");
    let p = r"D:\Games\Skyrim\Data\SKSE\Plugins\";
    write(&log, format!("checking plugin {p}\\Fixes.dll\nchecking plugin {p}\\Manual.dll\nplugin {p}\\Fixes.dll (00000001 Fixes 00000002) disabled, address library needs to be updated 00000000 (handle 0)\n"));
    let at = |t: SystemTime| fs::File::options().write(true).open(&log).unwrap().set_modified(t).unwrap();
    at(SystemTime::now() + Duration::from_secs(3600));
    assert!(found(&inst).contains(&("script_extender_plugin".into(), "Fixes".into())));
    let msg = health::check(&inst).unwrap().into_iter().find(|i| i.kind == "script_extender_plugin").unwrap().message;
    assert_eq!(msg, "Fixes.dll didn't load in the last game run: disabled, address library needs to be updated (Script extender log…)");
    let all = logs::script_extender_log(&inst, true).unwrap().plugins;
    assert_eq!(all.iter().map(|p| (p.file.as_str(), p.mod_id.as_deref())).collect::<Vec<_>>(), [("Fixes.dll", Some("Fixes")), ("Manual.dll", None)]);
    // A disabled mod's plugin won't load anyway.
    ops::set_mods_enabled(&inst, &["Fixes".into()], false).unwrap();
    assert!(!found(&inst).iter().any(|f| f.0 == "script_extender_plugin"));
    ops::set_mods_enabled(&inst, &["Fixes".into()], true).unwrap();
    // A mod installed or updated after the run isn't blamed for it.
    at(SystemTime::now() - Duration::from_secs(3600));
    assert!(!found(&inst).iter().any(|f| f.0 == "script_extender_plugin"));
    assert!(logs::script_extender_log(&inst, true).unwrap().plugins[0].changed);
}

#[test]
fn reinstall_relinks_without_external_changes() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    write(&tmp.path().join("A/meshes/a.nif"), "a-mesh v2");
    let id = &inst.profile().unwrap().mods[0].id.clone();
    install::reinstall(&inst, id, None).unwrap();
    let plan = deploy::deploy(&inst, false).unwrap();
    assert!(plan.external_changes.is_empty(), "{plan:?}");
    assert_eq!(read(&game.join("Data/meshes/a.nif")), "a-mesh v2");
    assert!(inst.mod_files(rmm_core::instance::OVERWRITE).unwrap().is_empty());

    // Reinstalled again and the trash emptied before deploying: the old files are gone from
    // trash/, but the links are still the ones rmm made, so they are relinked, not captured.
    write(&tmp.path().join("A/meshes/a.nif"), "a-mesh v3");
    install::reinstall(&inst, id, None).unwrap();
    install::empty_trash(&inst).unwrap();
    let plan = deploy::deploy(&inst, false).unwrap();
    assert!(plan.external_changes.is_empty(), "{plan:?}");
    assert_eq!(read(&game.join("Data/meshes/a.nif")), "a-mesh v3");
    assert!(inst.mod_files(rmm_core::instance::OVERWRITE).unwrap().is_empty());
}

/// deploy.json from before rmm kept file ids: links are still told apart by identity, and the
/// next deploy records their ids.
#[test]
fn a_manifest_without_file_ids_still_checks_identity() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    let path = inst.root.join("deploy.json");
    let strip = || {
        let mut man: serde_json::Value = serde_json::from_str(&read(&path)).unwrap();
        for e in man["files"].as_object_mut().unwrap().values_mut() {
            for k in ["id", "dir"] {
                assert_ne!(e.as_object_mut().unwrap().remove(k).expect("deploy.json keeps file ids"), 0);
            }
        }
        fs::write(&path, man.to_string()).unwrap();
    };
    strip();
    assert!(deploy::deploy(&inst, true).unwrap().is_noop());
    assert!(deploy::deploy(&inst, false).unwrap().is_noop());
    strip();
    // Replaced with the same size and mtime.
    let sky = game.join("Data/textures/sky.dds");
    let mtime = fs::metadata(&sky).unwrap().modified().unwrap();
    fs::remove_file(&sky).unwrap();
    write(&sky, "Z");
    fs::File::options().write(true).open(&sky).unwrap().set_modified(mtime).unwrap();
    assert_eq!(deploy::deploy(&inst, false).unwrap().external_changes, vec!["Data/textures/sky.dds"]);
}

/// A deploy that fails half-way records what it did. Otherwise the next deploy takes rmm's own
/// links for the user's files and backs them up over the vanilla backup, and purge leaves mod
/// files in the game folder.
#[test]
fn a_failed_deploy_keeps_its_record() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (game, src) = (tmp.path().join("game"), tmp.path().join("A"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/textures/sky.dds"), "vanilla");
    // Linked in path order: a.nif, then sky.dds over vanilla, then z.dds, which fails.
    write(&src.join("meshes/a.nif"), "a");
    write(&src.join("textures/sky.dds"), "A");
    write(&src.join("zz/z.dds"), "z");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &src, None, true, None).unwrap();
    write(&game.join("Data/zz"), "a file where a folder goes");
    assert!(deploy::deploy(&inst, false).is_err());

    fs::remove_file(game.join("Data/zz")).unwrap();
    let plan = deploy::deploy(&inst, false).unwrap();
    assert!(plan.backup.is_empty() && plan.external_changes.is_empty(), "{plan:?}");
    assert_eq!(read(&game.join("Data/zz/z.dds")), "z");
    deploy::purge(&inst, false).unwrap();
    assert_eq!(read(&game.join("Data/textures/sky.dds")), "vanilla");
    assert!(!game.join("Data/meshes").exists() && !game.join("Data/zz").exists());
}

/// Undo and redo move a mod's folder with it: redoing an uninstall or undoing an install puts it
/// in trash/ rather than leaving an installed mod that no list shows, unless another profile
/// lists it.
#[test]
fn undo_and_redo_move_mod_folders() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (inst, _) = deployed_setup(tmp.path());
    let id = inst.profile().unwrap().mods[0].id.clone();
    install::uninstall(&inst, &id).unwrap();
    inst.undo().unwrap();
    assert!(inst.mod_dir(&id).is_dir());
    inst.redo().unwrap();
    assert!(!inst.mod_dir(&id).exists() && inst.root.join("trash").join(&id).is_dir());
    inst.undo().unwrap();
    inst.undo().unwrap(); // the install
    assert!(!inst.mod_dir(&id).exists());
    inst.redo().unwrap();
    assert!(inst.mod_dir(&id).join("meshes/a.nif").is_file());

    inst.create_profile("Other", true).unwrap();
    inst.undo().unwrap();
    assert!(inst.mod_dir(&id).is_dir(), "the other profile still lists it");

    // Emptying the trash frees the space; undo then leaves the mod off the list.
    inst.redo().unwrap();
    install::uninstall(&inst, &id).unwrap();
    assert!(install::trash(&inst).unwrap().bytes > 0);
    assert!(inst.restore_preview(0).unwrap().added.contains(&id));
    install::empty_trash(&inst).unwrap();
    assert_eq!(install::trash(&inst).unwrap().entries, 0);
    // Going back can't bring it now, so neither the preview nor the history offers it.
    assert!(inst.restore_preview(0).unwrap().added.is_empty());
    assert!(inst.history_details().unwrap()[0].change.removed.is_empty());
    inst.undo().unwrap();
    assert!(inst.profile().unwrap().mods.iter().all(|m| m.id != id));
    deploy::deploy(&inst, false).unwrap();
}

/// Disabling a mod leaves its plugin on while another enabled mod provides the same file.
#[test]
fn disabling_a_mod_keeps_plugins_other_mods_provide() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    for m in ["Old", "Fixed"] {
        write(&tmp.path().join(m).join("Shared.esp"), plugin_bytes(&["Skyrim.esm"], false));
    }
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &tmp.path().join("Old"), None, true, None).unwrap();
    install::install(&inst, &tmp.path().join("Fixed"), None, true, None).unwrap();
    let on = |inst: &Instance| inst.profile().unwrap().plugins.iter().find(|p| p.name == "Shared.esp").unwrap().enabled;
    ops::set_mods_enabled(&inst, &["Old".into()], false).unwrap();
    assert!(on(&inst));
    ops::set_mods_enabled(&inst, &["Fixed".into()], false).unwrap();
    assert!(!on(&inst));
}

#[test]
fn purge_without_manifest_finds_links_by_identity() {
    isolate();
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
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (inst, game) = deployed_setup(tmp.path());
    write(&game.join("Data/vortex.deployment.json"), "{}");
    assert!(deploy::deploy(&inst, true).unwrap_err().to_string().contains("Vortex"));
    assert!(has_issue(&inst, "deploy_blocked"));
}

#[cfg(windows)]
#[test]
fn deploy_refuses_while_the_game_runs() {
    isolate();
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
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (game, ini) = (tmp.path().join("game"), tmp.path().join("My Games/Fallout4"));
    write(&game.join("Fallout4.exe"), "exe");
    write(&tmp.path().join("M/textures/x.dds"), "x");
    let mut inst = Instance::create(&tmp.path().join("inst"), Game::Fallout4, &game, &tmp.path().join("local")).unwrap();
    inst.config.ini_path = Some(ini.clone());
    install::install(&inst, &tmp.path().join("M"), None, true, None).unwrap();
    let kinds = |inst: &Instance| health::fallout4_ini(inst).unwrap().into_iter().map(|i| i.kind).collect::<Vec<_>>();

    assert!(kinds(&inst).is_empty(), "no INIs yet: the game has never run");
    // In the case the game uses under Proton; found either way.
    write(&ini.join("Fallout4.INI"), "[General]\nsTestFile1=Foo.esp\n[Archive]\nsResourceDataDirsFinal=STRINGS\\\n");
    assert_eq!(kinds(&inst), vec!["ini_test_file", "archive_invalidation"]);

    write(&ini.join("Fallout4Custom.ini"), "\u{feff}[General]\nsTestFile1=\n[Archive]\nbInvalidateOlderFiles=1\nsResourceDataDirsFinal=\n");
    assert!(kinds(&inst).is_empty());

    // General BA2 archives over the old limit; texture archives don't count.
    let ba2 = |inst: &Instance| health::check(inst).unwrap().into_iter().find(|i| i.kind == "ba2_limit").map(|i| (i.severity, i.message));
    for i in 0..256 {
        write(&game.join(format!("Data/CC{i:03} - Main.ba2")), "");
        write(&game.join(format!("Data/CC{i:03} - Textures.ba2")), "");
    }
    assert_eq!(ba2(&inst), None);
    write(&game.join("Data/One more - Main.ba2"), "");
    let (severity, message) = ba2(&inst).unwrap();
    assert_eq!(severity, "warning");
    assert!(message.starts_with("257 general BA2 archives"), "{message}");
}

/// Several plugins turned off at once are one undo step, and undo turns them all back on.
#[test]
fn several_plugins_in_one_step() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    for m in ["A", "B"] {
        write(&tmp.path().join(m).join(format!("{m}.esp")), plugin_bytes(&[], false));
        install::install(&inst, &tmp.path().join(m), None, true, None).unwrap();
    }
    let on = |inst: &Instance| inst.profile().unwrap().plugins.iter().filter(|p| p.enabled).map(|p| p.name.clone()).collect::<Vec<_>>();
    assert_eq!(on(&inst), ["A.esp", "B.esp"]);
    let steps = inst.history().unwrap().len();
    ops::set_plugins_enabled(&inst, &["a.esp".into(), "B.esp".into()], false).unwrap();
    assert!(on(&inst).is_empty());
    assert_eq!((inst.history().unwrap().len(), inst.history().unwrap()[0].as_str()), (steps + 1, "disable 2 plugins"));
    inst.undo().unwrap();
    assert_eq!(on(&inst), ["A.esp", "B.esp"]);
}

/// A new playthrough copies the profile and its INIs, with saves of its own; saves are copied and
/// moved between profiles with their co-saves.
#[test]
fn playthrough_and_moving_saves() {
    isolate();
    use rmm_core::saves;
    let tmp = tempfile::tempdir().unwrap();
    let (game, ini) = (tmp.path().join("game"), tmp.path().join("My Games/Skyrim Special Edition"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&tmp.path().join("M/M.esp"), plugin_bytes(&[], false));
    let mut inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    inst.config.ini_path = Some(ini.clone());
    install::install(&inst, &tmp.path().join("M"), None, true, None).unwrap();
    ops::set_profile_options(&inst, Some(true), None).unwrap();
    write(&inst.root.join("profiles/default/ini/Skyrim.ini"), "[General]\r\nsLanguage=ENGLISH\r\n");
    for f in ["a.ess", "a.skse", "b.ess", "notes.txt"] {
        write(&ini.join("Saves").join(f), f);
    }

    ops::new_playthrough(&mut inst, "Run 2").unwrap();
    let p = inst.profile().unwrap();
    assert_eq!((inst.config.profile.as_str(), p.local_inis, p.local_saves), ("Run 2", true, true));
    assert!(p.mods.len() == 1 && p.mods[0].enabled, "the same mods: {:?}", p.mods);
    assert_eq!(read(&inst.root.join("profiles/Run 2/ini/Skyrim.ini")), "[General]\r\nsLanguage=ENGLISH\r\n");
    let l = saves::list(&inst).unwrap();
    assert_eq!((l.dir, l.saves.len()), (Some(ini.join("Saves/Run 2")), 0), "its own, empty save folder");
    assert!(ops::new_playthrough(&mut inst, "run 2").is_err());

    inst.switch_profile("default").unwrap();
    let t = saves::transfer(&inst, &["a.ess".into()], "run 2", true).unwrap();
    assert_eq!((t.to, t.files), (ini.join("Saves/Run 2"), vec!["a.ess".to_string(), "a.skse".into()]));
    assert!(ini.join("Saves/a.ess").is_file() && ini.join("Saves/Run 2/a.skse").is_file(), "copied");
    assert!(saves::transfer(&inst, &["a.ess".into()], "Run 2", false).unwrap_err().to_string().contains("already has a.ess"));
    saves::transfer(&inst, &["b.ess".into()], "Run 2", false).unwrap();
    assert!(!ini.join("Saves/b.ess").exists() && ini.join("Saves/Run 2/b.ess").is_file(), "moved");
    for bad in ["../a.ess", "notes.txt", "gone.ess"] {
        assert!(saves::transfer(&inst, &[bad.into()], "Run 2", true).is_err(), "{bad}");
    }
    assert!(saves::transfer(&inst, &["a.ess".into()], "default", true).is_err(), "the active profile");
    inst.create_profile("Shared", true).unwrap();
    assert!(saves::transfer(&inst, &["a.ess".into()], "Shared", true).unwrap_err().to_string().contains("share one save folder"));
}

/// A full plugin whose new records fit a light plugin is an ESL candidate.
#[test]
fn profile_inis_and_saves() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (game, ini) = (tmp.path().join("game"), tmp.path().join("My Games/Skyrim Special Edition"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let mut inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    // The user's own load order, from another manager: a purge puts it back.
    write(&tmp.path().join("local/Plugins.txt"), "*Theirs.esp\r\n");
    let (mine, my_prefs, game_prefs) = ("[General]\r\nsLanguage=ENGLISH\r\n", "[Display]\r\niSize W=1920\r\n", "[Display]\r\niSize W=1280\r\n");
    write(&ini.join("Skyrim.ini"), mine);
    write(&ini.join("SkyrimPrefs.ini"), my_prefs);
    let at = |inst: &mut Instance| inst.config.ini_path = Some(ini.clone());
    at(&mut inst);
    let (state, prof) = (inst.root.join("ini-state.json"), inst.root.join("profiles/default/ini"));

    deploy::deploy(&inst, false).unwrap();
    assert_ne!(read(&tmp.path().join("local/Plugins.txt")), "*Theirs.esp\r\n");

    ops::set_profile_options(&inst, Some(true), Some(true)).unwrap();
    deploy::deploy(&inst, false).unwrap();
    // The profile starts from the user's INIs; its saves get their own folder.
    assert_eq!(read(&prof.join("Skyrim.ini")), mine);
    assert_eq!(read(&ini.join("Skyrim.ini")), "[General]\r\nsLocalSavePath=Saves\\default\\\r\nsLanguage=ENGLISH\r\n");
    // What the game changes (or adds) goes back to the profile, without rmm's save path.
    write(&ini.join("SkyrimPrefs.ini"), game_prefs);
    write(&ini.join("SkyrimCustom.ini"), "[Papyrus]\r\nbEnableLogging=1\r\n");
    deploy::deploy(&inst, false).unwrap();
    assert_eq!((read(&prof.join("Skyrim.ini")), read(&prof.join("SkyrimPrefs.ini"))), (mine.into(), game_prefs.into()));
    assert!(prof.join("SkyrimCustom.ini").exists());

    // A profile without the options gets the user's own INIs back.
    inst.create_profile("Plain", false).unwrap();
    inst.switch_profile("Plain").unwrap();
    at(&mut inst);
    deploy::deploy(&inst, false).unwrap();
    assert_eq!((read(&ini.join("Skyrim.ini")), read(&ini.join("SkyrimPrefs.ini"))), (mine.into(), my_prefs.into()));
    assert!(!ini.join("SkyrimCustom.ini").exists());

    // Back, renamed while deployed, then purged: the change lands in the renamed profile and
    // My Games is the user's again.
    inst.switch_profile("default").unwrap();
    at(&mut inst);
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(read(&ini.join("SkyrimPrefs.ini")), game_prefs);
    inst.rename_profile("default", "Main").unwrap();
    at(&mut inst);
    write(&ini.join("SkyrimPrefs.ini"), "[Display]\r\niSize W=800\r\n");
    deploy::purge(&inst, false).unwrap();
    assert_eq!(read(&inst.root.join("profiles/Main/ini/SkyrimPrefs.ini")), "[Display]\r\niSize W=800\r\n");
    assert_eq!((read(&ini.join("Skyrim.ini")), read(&ini.join("SkyrimPrefs.ini"))), (mine.into(), my_prefs.into()));
    assert!(!ini.join("SkyrimCustom.ini").exists() && !state.exists());
    assert_eq!(fs::read_dir(inst.root.join("ini-backup")).unwrap().count(), 0);
    assert_eq!(read(&tmp.path().join("local/Plugins.txt")), "*Theirs.esp\r\n");
}

#[test]
fn esl_candidates() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (inst, _) = deployed_setup(tmp.path());
    write(&tmp.path().join("Small/Small.esp"), plugin_bytes(&[], false));
    write(&tmp.path().join("Small/Master.esm"), plugin_bytes(&[], true));
    install::install(&inst, &tmp.path().join("Small"), None, true, None).unwrap();
    let esl = health::check(&inst).unwrap().into_iter().find(|i| i.kind == "esl_candidates").unwrap();
    assert_eq!(esl.severity, "info");
    assert!(esl.message.starts_with("1 full plugins could be flagged light (ESL), which frees full slots: Small.esp."), "{}", esl.message);
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
              <image path="fomod\Images\Red.PNG"/>
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
    write(&dir.join("fomod/images/red.png"), "png");
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
    isolate();
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

    // Images resolve inside the installer whatever their case; nothing outside it does.
    assert!(install::has_fomod(&inst, &src).unwrap());
    let image = opts[2].image.as_deref().unwrap();
    assert_eq!(read(&install::fomod_image(&inst, &src, image).unwrap()), "png");
    for bad in ["..\\F\\fomod\\images\\red.png", "fomod\\nope.png", "fomod\\images"] {
        assert!(install::fomod_image(&inst, &src, bad).is_err(), "{bad}");
    }

    let blue = choices(serde_json::json!({"options": [{"name": "1 Colour", "groups": [{"name": "Colour", "choices": [{"name": "Blue"}]}]}]}));
    let walk = install::fomod_options(&inst, &src, Some(&blue)).unwrap();
    assert_eq!(walk.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["1 Colour", "2 Extras"]);

    // Choices for a step these choices hide are reported, not silently dropped.
    let hidden = choices(serde_json::json!({"options": [{"name": "2 Extras", "groups": [{"name": "Extras", "choices": [{"name": "Glow"}]}]}]}));
    let w = install::fomod_options(&inst, &src, Some(&hidden)).unwrap();
    assert!(w.warnings.iter().any(|x| x.contains("'2 Extras' weren't used")), "{:?}", w.warnings);

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

    // Reinstall with other choices: same id, place and name; the old files go to trash/, and
    // undo swaps them back.
    ops::set_mods_enabled(&inst, std::slice::from_ref(&r.id), false).unwrap();
    assert_eq!(ops::rename_mod(&inst, &r.id, " Red armour ").unwrap(), "Red");
    let order = inst.profile().unwrap().mods;
    let again = install::reinstall(&inst, &r.id, Some(&picks)).unwrap();
    assert_eq!((again.id.as_str(), again.name.as_str()), (r.id.as_str(), "Red armour"));
    assert_eq!(inst.profile().unwrap().mods, order);
    assert_eq!(inst.mod_files(&r.id).unwrap(), files);
    assert_eq!(inst.mod_meta(&r.id).fomod.unwrap().options[1].groups[0].choices[0].name, "Glow");
    assert!(inst.root.join("trash/.replaced").join(format!("{}-1", r.id)).join("RedPatch.esp").is_file());
    let now = inst.mod_files(&r.id).unwrap();
    assert_eq!(inst.undo().unwrap().as_deref(), Some(format!("reinstall {}", r.id).as_str()));
    assert!(inst.mod_dir(&r.id).join("RedPatch.esp").is_file(), "the old files are back");
    inst.redo().unwrap();
    assert_eq!(inst.mod_files(&r.id).unwrap(), now);
    // Without choices it keeps the stored ones.
    assert_eq!(install::reinstall(&inst, &r.id, None).unwrap().files, 5);
    assert_eq!(ops::list_mods(&inst).unwrap().iter().find(|m| m.id == r.id).unwrap().name, "Red armour");
    assert!(ops::rename_mod(&inst, &r.id, "  ").is_err());
    // A plain archive and a folder reinstall too.
    let plain = tmp.path().join("Plain.zip");
    {
        let mut z = zip::ZipWriter::new(fs::File::create(&plain).unwrap());
        z.start_file("textures/plain.dds", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"plain").unwrap();
        z.finish().unwrap();
    }
    let pr = install::install(&inst, &plain, None, true, None).unwrap();
    assert_eq!(install::reinstall(&inst, &pr.id, None).unwrap().files, 1);
    let fr = install::install(&inst, &tmp.path().join("F wrapper/F/base"), Some("Folder"), true, None).unwrap();
    assert_eq!(install::reinstall(&inst, &fr.id, None).unwrap().files, fr.files);
    fs::remove_file(&zip_path).unwrap();
    assert!(install::reinstall(&inst, &r.id, None).unwrap_err().to_string().contains("is gone"));
    assert!(inst.mod_dir(&r.id).join("Core.esp").is_file());
}

/// RAR goes through Windows' own tar.exe (libarchive).
#[cfg(windows)]
#[test]
fn installs_rar_archives() {
    isolate();
    // RAR5, stored, made with WinRAR 7 (`rar a -m0 -ma5`): textures/a.dds = "from rar".
    const RAR5: &[u8] = &[
        0x52, 0x61, 0x72, 0x21, 0x1a, 0x07, 0x01, 0x00, 0x33, 0x92, 0xb5, 0xe5, 0x0a, 0x01, 0x05, 0x06, 0x00, 0x05, 0x01, 0x01, 0x80, 0x80, 0x00, 0xdd,
        0xe8, 0x15, 0x80, 0x1e, 0x02, 0x02, 0x88, 0x00, 0x04, 0x88, 0x00, 0x20, 0xd2, 0xe0, 0xfb, 0xa5, 0x80, 0x00, 0x00, 0x0e, 0x74, 0x65, 0x78, 0x74,
        0x75, 0x72, 0x65, 0x73, 0x2f, 0x61, 0x2e, 0x64, 0x64, 0x73, 0x66, 0x72, 0x6f, 0x6d, 0x20, 0x72, 0x61, 0x72, 0x65, 0xd9, 0x50, 0x7a, 0x12, 0x02,
        0x02, 0x00, 0x01, 0x00, 0x10, 0x80, 0x00, 0x00, 0x08, 0x74, 0x65, 0x78, 0x74, 0x75, 0x72, 0x65, 0x73, 0x1d, 0x77, 0x56, 0x51, 0x03, 0x05, 0x04,
        0x00,
    ];
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let rar = tmp.path().join("Sky Textures.rar");
    fs::write(&rar, RAR5).unwrap();
    let r = install::install(&inst, &rar, None, true, None).unwrap();
    assert_eq!(read(&inst.mod_dir(&r.id).join("textures/a.dds")), "from rar");
    // Not a RAR inside: tar's message comes back.
    fs::write(&rar, "not an archive").unwrap();
    let e = install::install(&inst, &rar, None, true, None).unwrap_err();
    assert!(format!("{e:#}").contains("RAR:"), "{e:#}");
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
    isolate();
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

    // Per file: who provides it, in load order, and who wins.
    let files = conflicts::file_conflicts(&inst, &inst.profile().unwrap(), "A").unwrap();
    let row = |f: &conflicts::FileConflict| format!("{} {} {} {}", f.file, f.mods.join(" "), f.winner, f.archives.join("|"));
    assert_eq!(files.iter().map(row).collect::<Vec<_>>(), ["meshes/m.nif A C C A.bsa|", "textures/x.dds A B B A.bsa|B - Textures.bsa"]);

    // The Data view: folders with file counts, files with who provides them, the game's own files.
    write(&game.join("Data/Video/intro.bik"), "the game's");
    let view = |dir: &str, find: &str, conflicts_only: bool, archives: bool| {
        let q = conflicts::DataQuery { dir: dir.into(), find: find.into(), conflicts_only, archives };
        let rows = conflicts::data_view(&inst, &inst.profile().unwrap(), &q).unwrap();
        rows.iter()
            .map(|e| match e.dir {
                true => format!("{}/ {:?}", e.name, e.files),
                false => format!("{} {}{}", e.name, e.mods.join(","), e.archive.as_ref().map(|a| format!(" in {a}")).unwrap_or_default()),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(view("", "", false, false), ["meshes/ Some(1)", "Video/ None", "A.bsa A", "A.esp A", "B - Textures.bsa B", "B.esp B", "D.bsa D", "Skyrim.esm "]);
    assert_eq!(view("", "", false, true)[..3], ["meshes/ Some(1)", "textures/ Some(2)", "Video/ None"], "D.bsa isn't loaded");
    assert_eq!(view("Textures", "", false, true), ["a.dds A in A.bsa", "x.dds A,B in B - Textures.bsa"]);
    assert_eq!(view("meshes", "", false, true), ["m.nif A,C"], "C's loose file wins over A's archive");
    assert_eq!(view("", "", true, true), ["meshes/ Some(1)", "textures/ Some(1)"]);
    assert_eq!(view("", "X.DDS", false, true), ["textures/x.dds A,B in B - Textures.bsa"]);
    assert!(conflicts::data_view(&inst, &inst.profile().unwrap(), &conflicts::DataQuery { dir: "../..".into(), ..Default::default() }).is_err());

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
    isolate();
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
    isolate();
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
    rules::set_rule(&inst, &ModRule { id: "gen Output".into(), after: vec!["Plain".into()], ..Default::default() }).unwrap();
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

/// xEdit-style patching: the output mod stays deployed while the tool runs, a plugin it saves (a
/// new file renamed over the deployed link, as xEdit does) goes back to its mod, new files go into
/// the chosen mod, and undo and redo swap the mods' files.
#[cfg(windows)]
#[test]
fn tool_edits_go_back_to_their_mods() {
    isolate();
    use rmm_core::tools::{self, ToolSpec};
    use std::collections::BTreeMap;
    let tmp = tempfile::tempdir().unwrap();
    let (game, local, src, root) = (tmp.path().join("game"), tmp.path().join("local"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let (v1, v2) = (plugin_bytes(&[], false), plugin_bytes(&["Skyrim.esm"], false));
    write(&src.join("Patches/Patch.esp"), &v1);
    write(&src.join("Big/meshes/x.nif"), "x");
    write(&src.join("files/Patch2.esp"), &v2);
    write(&src.join("files/New.esp"), &v1);
    write(
        &src.join("tool.bat"),
        [
            r"@echo off",
            r#"if exist "%~2\Patch.esp" echo seen> "%~1\seen.txt""#,
            r#"copy /y "%~3\Patch2.esp" "%~2\Patch.esp.tmp" >nul"#,
            r#"move /y "%~2\Patch.esp.tmp" "%~2\Patch.esp" >nul"#,
            r#"if not exist "%~2\New.esp" copy /y "%~3\New.esp" "%~2\New.esp" >nul"#,
            r"echo done",
        ]
        .join("\r\n"),
    );
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &local).unwrap();
    install::install(&inst, &src.join("Patches"), None, true, None).unwrap();
    install::install(&inst, &src.join("Big"), None, true, None).unwrap();
    deploy::deploy(&inst, false).unwrap();
    let spec = |out: &str| ToolSpec {
        exe: src.join("tool.bat"),
        args: Some(vec!["{output}".into(), "{data}".into(), "{files}".into()]),
        vars: BTreeMap::from([("files".to_string(), src.join("files").display().to_string())]),
        output_mod: Some(out.into()),
        edit_in_place: Some(true),
        ..Default::default()
    };
    tools::add(&inst, "xe", spec("Patches")).unwrap();

    let plan = tools::run(&inst, "xe", &BTreeMap::new(), true, None).unwrap();
    assert!(plan.edit_in_place && plan.previous.as_deref() == Some("Patches") && plan.warnings.is_empty(), "{plan:?}");
    let r = tools::run(&inst, "xe", &BTreeMap::new(), false, None).unwrap();
    assert_eq!((r.into_mods.join(" "), r.mod_id.as_deref()), ("Patches: Patch.esp".into(), Some("Patches")), "{r:?}");
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let patches = root.join("mods/Patches");
    assert_eq!(fs::read(patches.join("Patch.esp")).unwrap(), v2);
    assert_eq!(fs::read(patches.join("New.esp")).unwrap(), v1);
    assert!(patches.join("seen.txt").is_file(), "the output mod stayed deployed while the tool ran");
    assert!(same_file::is_same_file(game.join("Data/Patch.esp"), patches.join("Patch.esp")).unwrap());
    assert_eq!(order(&inst), "Patches+ Big+", "no mod added or disabled");
    assert!(inst.profile().unwrap().plugins.iter().any(|e| e.name == "New.esp" && e.enabled));

    // Undo puts the mod's files back as they were; redo brings the edits back.
    inst.undo().unwrap();
    assert_eq!(fs::read(patches.join("Patch.esp")).unwrap(), v1);
    assert!(!patches.join("New.esp").exists() && !inst.mod_files("Patches").unwrap().contains(&"New.esp".to_string()));
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(fs::read(game.join("Data/Patch.esp")).unwrap(), v1);
    assert!(!game.join("Data/New.esp").exists());
    inst.redo().unwrap();
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(fs::read(game.join("Data/Patch.esp")).unwrap(), v2);
    assert!(inst.mod_files("Patches").unwrap().contains(&"New.esp".to_string()));

    // A name no mod has: new files make that mod, at the end of the list.
    tools::add(&inst, "xe", spec("Fresh")).unwrap();
    let r = tools::run(&inst, "xe", &BTreeMap::new(), false, None).unwrap();
    assert_eq!(r.mod_id.as_deref(), Some("Fresh"), "{r:?}");
    assert!(root.join("mods/Fresh/seen.txt").is_file());
    assert_eq!(order(&inst), "Patches+ Big+ Fresh+");
}

/// A JSON settings file is merged before the run (other keys and a BOM survive), kept files carry
/// over from the previous output, and a run that adds a plugin is sorted with the cached masterlist.
#[cfg(windows)]
#[test]
fn tool_settings_kept_files_and_sort() {
    isolate();
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

    // A tool installed as a mod runs from Data, so its settings file is in Data: rmm sets it and
    // puts it back, and it isn't the tool's output.
    write(&src.join("m/tool.bat"), "@echo off\r\nmkdir \"%~1\" 2>nul\r\necho x> \"%~1\\out.txt\"");
    install::install(&inst, &src.join("m"), Some("Tool"), true, None).unwrap();
    rmm_core::deploy::deploy(&inst, false).unwrap();
    let set = BTreeMap::from([("games.SkyrimSE.outputPath".to_string(), "{output}".into())]);
    let spec = ToolSpec { exe: game.join("Data/tool.bat"), args: Some(vec!["{output}".into()]), settings: Some(Settings { file: "Settings.json".into(), set }), ..Default::default() };
    tools::add(&inst, "gen2", spec).unwrap();
    let r3 = tools::run(&inst, "gen2", &BTreeMap::new(), false, None).unwrap();
    assert!(r3.warnings.is_empty() && r3.changed.is_empty(), "{r3:?}");
    assert_eq!(read(&root.join(format!("mods/{}/out.txt", r3.mod_id.as_deref().unwrap()))).trim(), "x");
    assert!(!root.join(format!("mods/{}/Settings.json", r3.mod_id.as_deref().unwrap())).exists());
    assert!(!game.join("Data/Settings.json").exists(), "rmm made it, and it's empty again");
}

/// Launch reads the game's version resource, refuses a script extender built for another version,
/// and starts the matching loader from the game folder. PING.EXE stands in for game and loader.
#[cfg(windows)]
#[test]
fn launch_checks_the_script_extender() {
    isolate();
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

/// test_run without the harness plugin, and the crash and Papyrus logs it reads back to mods.
#[cfg(windows)]
#[test]
fn test_run_reads_logs_back_to_mods() {
    isolate();
    use rmm_core::{harness, logs};
    let tmp = tempfile::tempdir().unwrap();
    let (mut inst, game) = deployed_setup(tmp.path());
    let my = tmp.path().join("My Games");
    inst.config.ini_path = Some(my.clone());
    write(&tmp.path().join("Smp/SKSE/Plugins/hdtSMP64.dll"), "dll");
    write(&tmp.path().join("Smp/scripts/MyScript.pex"), "pex");
    install::install(&inst, &tmp.path().join("Smp"), None, true, None).unwrap();
    // Plugins' own logs that start with "Crash" aren't crash logs.
    write(&my.join("SKSE/CrashLogger.log"), "loaded");
    write(&my.join("SKSE/CrashFixCompendium.log"), "loaded");
    assert!(logs::read_crash_log(&inst, None).unwrap_err().to_string().contains("no crash logs"));

    // The stand-in game exits at once; a crash log written during the run fails it.
    fs::copy(r"C:\Windows\System32\PING.EXE", game.join("SkyrimSE.exe")).unwrap();
    let log = my.join("SKSE/crash-2026-11-16-22-00-00.log");
    let writer = {
        let log = log.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            write(&log, "Skyrim SSE v1.6.1170
CrashLoggerSSE v1-19-0-0

Unhandled exception \"EXCEPTION_ACCESS_VIOLATION\" at 0x1

CALL STACK ([P]robable / [S]tack scan):
	[ 0] 0x1 hdtSMP64.dll+0001

REGISTERS:
	RCX File: \"Skyrim.esm\" Model: \"meshes\\a.nif\"
");
        })
    };
    let r = harness::test_run(&inst, &harness::Scenario { timeout: Some(3), ..Default::default() }).unwrap();
    writer.join().unwrap();
    assert_eq!(r.mode, "process");
    assert!(!r.passed);
    assert_eq!(r.summary, "the game crashed; suspects: Smp, A");
    let crash = r.crash.unwrap();
    assert_eq!(crash.path, log);
    assert_eq!(crash.suspects.iter().map(|s| (s.name.as_str(), s.mod_id.as_deref())).collect::<Vec<_>>(), [("hdtSMP64.dll", Some("Smp")), ("meshes\\a.nif", Some("A"))]);

    write(&my.join("Skyrim.ini"), "[Papyrus]
bEnableLogging=1
");
    write(&my.join("Logs/Script/Papyrus.0.log"), "[11/16/2026 - 10:00:00PM] error: Cannot call Foo() on a None object, aborting function call
stack:
	[alias X on quest Y (0A001234)].MyScript.OnUpdate() - \"MyScript.psc\" Line 42
[11/16/2026 - 10:00:01PM] warning: Assigning None to a non-object property
");
    let p = logs::read_papyrus_log(&inst, 10).unwrap();
    assert!(p.logging_enabled);
    assert_eq!((p.errors, p.warnings, p.tail.len()), (1, 1, 4));
    assert_eq!(p.messages, [("Cannot call Foo() on a None object, aborting function call".to_string(), 1)]);
    assert_eq!(p.scripts, [logs::ScriptHits { script: "MyScript".into(), mod_id: Some("Smp".into()), count: 1 }]);

    // What to try, a report to paste, and a support bundle without the Windows user name.
    use rmm_core::support;
    let crash = logs::read_crash_log(&inst, None).unwrap();
    assert!(crash.next_steps.iter().any(|s| s.starts_with("hdtSMP64.dll is a script extender plugin from Smp")), "{:?}", crash.next_steps);
    assert!(crash.next_steps.last().unwrap().contains("half of the 2 mods"), "{:?}", crash.next_steps);
    let text = support::crash_text(&inst, &crash);
    assert!(text.contains("- hdtSMP64.dll (call stack): Smp") && text.contains("What rmm suggests:"), "{text}");
    let b = support::bundle(&inst, Some(&tmp.path().join("out")), None).unwrap();
    assert!(b.path.starts_with(tmp.path().join("out")) && b.skipped.is_empty(), "{b:?}");
    for f in ["summary.md", "health.json", "modlist.lock.toml", "deploy-plan.json", "settings.json", "logs/crash-2026-11-16-22-00-00.log", "logs/Papyrus.0.log"] {
        assert!(b.files.iter().any(|x| x == f), "{f} in {:?}", b.files);
    }
    let mut zip = zip::ZipArchive::new(fs::File::open(&b.path).unwrap()).unwrap();
    let user = std::env::var("USERNAME").unwrap_or_default().to_lowercase();
    for i in 0..zip.len() {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut zip.by_index(i).unwrap(), &mut text).unwrap();
        let lower = text.to_lowercase();
        assert!(user.is_empty() || !(lower.contains(&format!("users\\{user}")) || lower.contains(&format!("users/{user}"))), "the user name in {text}");
    }
    let mut summary = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("summary.md").unwrap(), &mut summary).unwrap();
    assert!(summary.contains("- hdtSMP64.dll (call stack): Smp") && summary.contains("## Problems"), "{summary}");
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
    isolate();
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
    isolate();
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
    isolate();
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
    isolate();
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
    isolate();
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
    isolate();
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
    let rule = |id: &str, after: &[&str]| ModRule { id: id.into(), after: after.iter().map(|s| s.to_string()).collect(), ..Default::default() };
    let ids = |inst: &Instance| inst.profile().unwrap().mods.into_iter().map(|m| m.id).collect::<Vec<_>>().join(" ");
    let broken = |inst: &Instance| health::check(inst).unwrap().into_iter().filter(|i| i.kind == "mod_rule_broken").map(|i| format!("{}: {}", i.subject, i.message)).collect::<Vec<_>>();

    for bad in [rule("A", &["A"]), rule("A", &["Nope"]), rule("Nope", &["A"]), rule("A", &["<overwrite>"])] {
        assert!(rules::set_rule(&inst, &bad).is_err(), "{bad:?}");
    }
    assert_eq!(rules::set_rule(&inst, &rule("A", &["C", "C"])).unwrap(), None);
    assert_eq!(rules::rules(&inst).unwrap(), [rule("A", &["C"])]);
    assert_eq!(broken(&inst), ["A: should load after C (a mod rule); Sort by rules fixes the order"]);

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

    // Requires and incompatible: reported for enabled mods only, never blocking.
    let kinds = |inst: &Instance| health::check(inst).unwrap().into_iter().filter(|i| i.kind == "mod_requires" || i.kind == "mod_incompatible").map(|i| format!("{} {}", i.kind, i.subject)).collect::<Vec<_>>();
    let b = ModRule { id: "B".into(), requires: vec!["C".into()], incompatible: vec!["A".into()], ..Default::default() };
    rules::set_rule(&inst, &b).unwrap();
    assert_eq!(kinds(&inst), ["mod_incompatible B"]);
    set_mod_enabled(&inst, "C", false).unwrap();
    set_mod_enabled(&inst, "A", false).unwrap();
    assert_eq!(kinds(&inst), ["mod_requires B"]);
    set_mod_enabled(&inst, "B", false).unwrap();
    assert!(kinds(&inst).is_empty());
    let both = ModRule { id: "B".into(), requires: vec!["C".into()], incompatible: vec!["C".into()], ..Default::default() };
    assert!(rules::set_rule(&inst, &both).is_err());
    assert_eq!(rules::rules(&inst).unwrap()[1], b);

    // A new mod goes before generated tool output, and before a mod that loads after it.
    assert_eq!(ids(&inst), "B C A");
    write(&src.join("D/textures/d.dds"), "D");
    install::install(&inst, &src.join("D"), None, true, None).unwrap();
    assert_eq!(ids(&inst), "B C A D");
    let mut meta = inst.mod_meta("D");
    meta.source = Some("tool:gen".into());
    inst.write_mod_meta("D", &meta).unwrap();
    write(&src.join("E/textures/e.dds"), "E");
    install::install(&inst, &src.join("E"), None, true, None).unwrap();
    assert_eq!(ids(&inst), "B C A E D");
    fs::create_dir_all(inst.mod_dir("F")).unwrap(); // a rule can name a mod installed later
    rules::set_rule(&inst, &rule("C", &["F"])).unwrap();
    fs::remove_dir(inst.mod_dir("F")).unwrap();
    write(&src.join("F/textures/f.dds"), "F");
    assert_eq!(install::install(&inst, &src.join("F"), None, true, None).unwrap().id, "F");
    assert_eq!(ids(&inst), "B F C A E D");
    // An explicit priority wins.
    write(&src.join("G/textures/g.dds"), "G");
    install::install_at(&inst, &src.join("G"), None, true, None, Some(0)).unwrap();
    assert_eq!(ids(&inst), "G B F C A E D");
}

/// Separators head groups of mods; bulk enable, move and uninstall are one undo step each.
#[test]
fn separators_and_bulk_ops() {
    isolate();
    use rmm_core::ops::{self, add_separator, edit_separator, move_mods, set_mods_enabled};
    let tmp = tempfile::tempdir().unwrap();
    let (game, src, root) = (tmp.path().join("game"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    let mut inst = Instance::create(&root, Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    for id in ["A", "B", "C"] {
        write(&src.join(id).join(format!("{id}.esp")), plugin_bytes(&[], false));
        install::install(&inst, &src.join(id), None, true, None).unwrap();
    }
    let ids = |inst: &Instance| inst.profile().unwrap().mods.into_iter().map(|m| m.id).collect::<Vec<_>>().join(" | ");
    let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    assert!(add_separator(&inst, "  ", None).is_err());
    assert_eq!(add_separator(&inst, "Textures", Some(0)).unwrap(), "sep:Textures");
    assert_eq!(add_separator(&inst, "Textures", None).unwrap(), "sep:Textures (2)");
    assert_eq!(ids(&inst), "sep:Textures | A | B | C | sep:Textures (2)");
    let rows = ops::list_mods(&inst).unwrap();
    assert!(rows[0].separator && rows[0].name == "Textures" && rows[0].files == 0);
    assert_eq!(ops::instance_info(&inst).unwrap()["mods"], 3);

    // Bulk enable skips separators and takes the plugins along; a separator alone has no state.
    assert!(ops::set_mod_enabled(&inst, "sep:Textures", true).is_err());
    set_mods_enabled(&inst, &v(&["A", "sep:Textures", "B"]), false).unwrap();
    assert_eq!(inst.history().unwrap()[0], "disable 2 mods");
    let p = inst.profile().unwrap();
    assert_eq!(p.mods.iter().filter(|m| m.enabled).map(|m| m.id.as_str()).collect::<Vec<_>>(), ["C"]);
    assert_eq!(p.plugins.iter().filter(|e| e.enabled).map(|e| e.name.as_str()).collect::<Vec<_>>(), ["C.esp"]);

    // A block moves in list order, whatever order the ids come in.
    move_mods(&inst, &v(&["sep:Textures (2)", "C"]), 0).unwrap();
    assert_eq!(ids(&inst), "C | sep:Textures (2) | sep:Textures | A | B");
    assert!(move_mods(&inst, &v(&["Nope"]), 0).is_err());

    // Rename carries the colour; colours are validated.
    assert!(edit_separator(&inst, "A", Some("X"), None).is_err());
    assert!(edit_separator(&inst, "sep:Textures", None, Some("red")).is_err());
    assert!(edit_separator(&inst, "sep:Textures", Some("Textures (2)"), None).is_err());
    edit_separator(&inst, "sep:Textures", None, Some("#AABBCC")).unwrap();
    assert_eq!(edit_separator(&inst, "sep:Textures", Some("Gameplay"), None).unwrap(), "sep:Gameplay");
    let rows = ops::list_mods(&inst).unwrap();
    assert_eq!((rows[2].id.as_str(), rows[2].color.as_deref()), ("sep:Gameplay", Some("#aabbcc")));

    // Separators survive a profile switch and never count as missing mods.
    inst.create_profile("Other", true).unwrap();
    inst.switch_profile("Other").unwrap();
    assert_eq!(ids(&inst), "C | sep:Textures (2) | sep:Gameplay | A | B");
    assert!(health::check(&inst).unwrap().iter().all(|i| !i.subject.starts_with("sep:")));
    assert!(deploy::deploy(&inst, true).is_ok());

    // Uninstalling a mod and a separator together is one step, and undo brings both back.
    install::uninstall_mods(&inst, &v(&["sep:Gameplay", "B"])).unwrap();
    assert_eq!(inst.history().unwrap()[0], "uninstall 2 mods");
    assert_eq!(ids(&inst), "C | sep:Textures (2) | A");
    assert!(!root.join("mods/B").exists() && inst.profile().unwrap().colors.is_empty());
    inst.undo().unwrap();
    assert_eq!(ids(&inst), "C | sep:Textures (2) | sep:Gameplay | A | B");
    assert!(root.join("mods/B").is_dir());
    assert_eq!(inst.profile().unwrap().colors["sep:Gameplay"], "#aabbcc");
}

#[test]
fn notes_colours_locks_and_restore() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (game, src, root) = (tmp.path().join("game"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    write(&src.join("A/textures/t.dds"), "A");
    write(&src.join("A/scripts/s.pex"), "pex");
    write(&src.join("B/textures/t.dds"), "B");
    write(&src.join("B/textures/b.dds"), "B");
    for id in ["A", "B", "C"] {
        write(&src.join(id).join(format!("{id}.esp")), plugin_bytes(&[], false));
        install::install(&inst, &src.join(id), None, true, None).unwrap();
    }
    let row = |id: &str| ops::list_mods(&inst).unwrap().into_iter().find(|m| m.id == id).unwrap();
    let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let plugins = || inst.profile().unwrap().plugins.into_iter().map(|e| e.name).collect::<Vec<_>>().join(" ");

    // Tags come from the files.
    assert_eq!(row("A").tags, ["plugins", "scripts", "textures"]);

    // Notes live with the mod; empty clears them. Writing them changes the time of mods/ (which
    // the window watches for changes from the CLI), not the mod's install date.
    let time = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
    let (mods, installed) = (time(&inst.mods_dir()), row("A").installed);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(ops::set_mod_note(&inst, "A", "  needs B  ").unwrap(), None);
    assert_eq!(row("A").notes.as_deref(), Some("needs B"));
    assert_ne!(time(&inst.mods_dir()), mods);
    assert_eq!(row("A").installed, installed);
    assert_eq!(ops::set_mod_note(&inst, "A", "").unwrap().as_deref(), Some("needs B"));
    assert_eq!(row("A").notes, None);
    assert!(ops::set_mod_note(&inst, "Nope", "x").is_err());

    // Any row can take a colour; undoable.
    assert!(ops::set_color(&inst, "A", "red").is_err());
    assert!(ops::set_color(&inst, "Nope", "#000000").is_err());
    ops::set_color(&inst, "A", "#FF0000").unwrap();
    assert_eq!(row("A").color.as_deref(), Some("#ff0000"));
    inst.undo().unwrap();
    assert_eq!(row("A").color, None);

    // A locked plugin stays put whatever else moves.
    ops::set_plugin_order(&inst, &v(&["C.esp", "A.esp", "B.esp"])).unwrap();
    ops::lock_plugin(&inst, "c.esp", true).unwrap();
    assert_eq!(inst.profile().unwrap().locks["C.esp"], 0);
    ops::set_plugin_order(&inst, &v(&["B.esp", "A.esp", "C.esp"])).unwrap();
    assert_eq!(plugins(), "C.esp B.esp A.esp");
    ops::lock_plugin(&inst, "C.esp", false).unwrap();
    ops::set_plugin_order(&inst, &v(&["A.esp", "B.esp", "C.esp"])).unwrap();
    assert_eq!(plugins(), "A.esp B.esp C.esp");
    assert!(ops::lock_plugin(&inst, "Nope.esp", true).is_err());

    // Loose per-file conflicts.
    let files = conflicts::file_conflicts(&inst, &inst.profile().unwrap(), "A").unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!((files[0].file.as_str(), files[0].mods.join(" "), files[0].winner.as_str()), ("textures/t.dds", "A B".into(), "B"));

    // History says what each change did; restore goes back several steps at once and redo returns.
    ops::set_mods_enabled(&inst, &v(&["B"]), false).unwrap();
    ops::move_mods(&inst, &v(&["C"]), 0).unwrap();
    let h = inst.history_details().unwrap();
    assert_eq!(h[0].change.moved, ["C"]);
    assert_eq!(h[1].change.disabled, ["B"]);
    assert!(h[1].change.moved.is_empty() && h[1].change.plugins == 1, "{:?}", h[1]);
    let preview = inst.restore_preview(1).unwrap();
    assert_eq!((preview.enabled.as_slice(), preview.moved.as_slice()), (v(&["B"]).as_slice(), v(&["C"]).as_slice()));
    assert_eq!(inst.restore(1).unwrap().len(), 2);
    let p = inst.profile().unwrap();
    assert_eq!(p.mods.iter().map(|m| format!("{}{}", m.id, if m.enabled { "" } else { "-" })).collect::<Vec<_>>().join(" "), "A B C");
    assert!(inst.restore(99).is_err());
    assert!(inst.redo().unwrap().is_some());
    assert!(!inst.profile().unwrap().mods[1].enabled);
}

#[test]
fn deploy_refuses_more_plugins_than_the_game_loads() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (game, src) = (tmp.path().join("game"), tmp.path().join("src"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    for i in 0..254 {
        write(&src.join(format!("Many/P{i:03}.esp")), plugin_bytes(&["Skyrim.esm"], false));
    }
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &src.join("Many"), None, true, None).unwrap();
    let e = deploy::deploy(&inst, true).unwrap_err().to_string();
    assert!(e.contains("255 full plugins are enabled"), "{e}");
    assert!(deploy::deploy(&inst, false).is_err());
    assert!(!game.join("Data/P000.esp").exists(), "nothing deployed");
    assert!(has_issue(&inst, "plugin_limit"));
    ops::set_plugin_enabled(&inst, "P253.esp", false).unwrap();
    deploy::deploy(&inst, false).unwrap();
    assert!(game.join("Data/P000.esp").exists());
}

fn order(inst: &Instance) -> String {
    inst.profile().unwrap().mods.iter().map(|m| format!("{}{}", m.id, if m.enabled { "+" } else { "" })).collect::<Vec<_>>().join(" ")
}

#[test]
fn imports_an_mo2_instance() {
    isolate();
    use rmm_core::import;
    let tmp = tempfile::tempdir().unwrap();
    let (game, mo2) = (tmp.path().join("game"), tmp.path().join("mo2"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&mo2.join("ModOrganizer.ini"), "[General]\r\ngameName=Skyrim Special Edition\r\nselected_profile=@ByteArray(Main)\r\n");
    write(&mo2.join("mods/A/A.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&mo2.join("mods/A/textures/a.dds"), "A");
    write(&mo2.join("mods/A/meta.ini"), "[General]\r\nmodid=123\r\nversion=1.0\r\ninstallationFile=A-123-1-0.7z\r\ncomments=keep this\r\nnotes=\"<html><head><style>p, li { white-space: pre-wrap; }</style></head><body>\\n<p>Use the <b>2K</b> option &amp; skip the rest.</p></body></html>\"\r\ncategory=\"12,1,\"\r\nnexusCategory=29\r\n[installedFiles]\r\n1\\modid=123\r\n1\\fileid=456\r\nsize=1\r\n");
    write(&mo2.join("categories.dat"), "1|Animations|51,4|0\r\n12|Weapons|3|0\r\n");
    write(&mo2.join("mods/B/meta.ini"), "[General]\r\nmodid=7\r\ncategory=\"-1,\"\r\nnexusCategory=29\r\n");
    write(&mo2.join("mods/B/B.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&mo2.join("mods/B/textures/a.dds"), "B");
    write(&mo2.join("mods/B/Root/enbseries.ini"), "enb");
    write(&mo2.join("mods/Visuals_separator/meta.ini"), "[General]\r\ncolor=@Variant(\\0\\0\\0\\x43\\x1\\xff\\xff\\xff\\xff\\x80\\x80\\0\\0\\0\\0)\r\n");
    write(&mo2.join("mods/C/meshes/c.nif"), "C");
    write(&mo2.join("profiles/Main/modlist.txt"), "# This file was automatically generated by Mod Organizer.\r\n+B\r\n-Visuals_separator\r\n-A\r\n*DLC: Dawnguard\r\n");
    write(&mo2.join("profiles/Main/plugins.txt"), "# This file was automatically generated by Mod Organizer.\r\n*B.esp\r\nA.esp\r\n");
    write(&mo2.join("profiles/Main/loadorder.txt"), "Skyrim.esm\r\nB.esp\r\nA.esp\r\n");
    write(&mo2.join("profiles/Main/lockedorder.txt"), "B.esp|1\r\n");
    write(&mo2.join("profiles/Alt/modlist.txt"), "+C\r\n");
    write(&mo2.join("overwrite/SKSE/Plugins/x.ini"), "x");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();

    assert_eq!(import::mo2_profiles(&mo2).unwrap(), ["Main", "Alt"], "the selected one first");
    assert_eq!((import::mo2_game(&mo2), import::mo2_game(&game)), (Some(Game::SkyrimSe), None));
    let dry = import::import_mo2(&inst, &mo2, None, true).unwrap();
    assert_eq!((dry.mods.join(" "), dry.separators, dry.plugins), ("A B C".into(), 1, 3));
    assert!(!inst.mod_dir("A").exists() && order(&inst).is_empty());

    let r = import::import_mo2(&inst, &mo2, None, false).unwrap();
    assert_eq!(r.linked + r.copied, 6, "{r:?}"); // 5 mod files and the overwrite file; meta.ini stays behind
    assert_eq!(order(&inst), "A sep:Visuals B+ C");
    assert_eq!(inst.mod_files("B").unwrap(), ["B.esp", "root/enbseries.ini", "textures/a.dds"]);
    let meta = inst.mod_meta("A");
    assert_eq!((meta.notes.as_deref(), meta.nexus.as_ref().map(|n| (n.mod_id, n.file_id, n.version.clone()))), (Some("keep this\n\nUse the 2K option & skip the rest."), Some((123, Some(456), Some("1.0".into())))));
    assert!(meta.source.unwrap().replace('\\', "/").ends_with("mo2/downloads/A-123-1-0.7z"));
    let p = inst.profile().unwrap();
    assert_eq!(p.plugins.iter().map(|e| format!("{}{}", e.name, if e.enabled { "+" } else { "" })).collect::<Vec<_>>().join(" "), "Skyrim.esm B.esp+ A.esp");
    assert_eq!(p.locks["B.esp"], 1);
    assert_eq!(p.colors.get("sep:Visuals").map(String::as_str), Some("#ff8000"), "MO2's separator colour");
    assert!(inst.root.join("overwrite/SKSE/Plugins/x.ini").is_file());
    assert_eq!(inst.history().unwrap()[0], "import MO2 profile Main");

    // Categories: MO2's own come over by name; without them the Nexus category shows once its name is known.
    let cats = |id: &str| ops::list_mods(&inst).unwrap().into_iter().find(|m| m.id == id).unwrap().categories.join(", ");
    assert_eq!(meta.categories, Some(vec!["Weapons".into(), "Animations".into()]));
    assert_eq!((inst.mod_meta("B").categories, inst.mod_meta("B").nexus.unwrap().category), (None, Some(29)));
    assert_eq!(cats("B"), "", "no Nexus names read yet");
    write(&inst.root.join("nexus-categories.json"), r#"{"29":"Armour"}"#);
    assert_eq!((cats("A"), cats("B")), ("Weapons, Animations".into(), "Armour".into()));
    ops::edit_categories(&inst, &["B".into()], &["Weapons".into(), " weapons ".into(), "".into()], &[]).unwrap();
    assert_eq!(cats("B"), "Armour, Weapons", "the Nexus category stays, duplicates and blanks don't count");
    ops::edit_categories(&inst, &["A".into(), "B".into()], &[], &["WEAPONS".into()]).unwrap();
    assert_eq!((cats("A"), cats("B")), ("Animations".into(), "Armour".into()));
    ops::edit_categories(&inst, &["B".into()], &[], &["Armour".into()]).unwrap();
    assert_eq!((cats("B"), inst.mod_meta("B").categories), ("".into(), Some(vec![])), "removed for good, not back to Nexus's");
    assert!(ops::edit_categories(&inst, &["A".into(), "sep:Visuals".into()], &["X".into()], &[]).is_err());
    assert_eq!(cats("A"), "Animations", "nothing changes when one id is wrong");

    // Again: nothing new to bring in.
    let again = import::import_mo2(&inst, &mo2, Some("Main"), false).unwrap();
    assert_eq!((again.mods.len(), again.existing.join(" ")), (0, "A B C".into()));
    assert!(import::import_mo2(&inst, &mo2, Some("Nope"), true).is_err());
}

#[test]
fn imports_all_of_an_mo2_instance() {
    isolate();
    use rmm_core::{import, instance::InstanceConfig, nexus, tools};
    use std::collections::BTreeMap;
    let tmp = tempfile::tempdir().unwrap();
    let (game, mo2, bin, my) = (tmp.path().join("game"), tmp.path().join("mo2"), tmp.path().join("bin"), tmp.path().join("mygames"));
    let slash = |p: &Path| p.display().to_string().replace('\\', "/");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("skse64_loader.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    for exe in ["SSEEdit.exe", "mytool.exe", "hidden.exe"] {
        write(&bin.join(exe), "exe");
    }
    let exes = [
        ("Skyrim Special Edition", format!("{}/SkyrimSE.exe", slash(&game)), ""),
        ("SKSE", format!("{}/skse64_loader.exe", slash(&game)), ""),
        ("SSEEdit", format!("{}/SSEEdit.exe", slash(&bin)), ""),
        ("BodySlide x64", format!("{}/mods/BodySlide/CalienteTools/BodySlide/BodySlide x64.exe", slash(&mo2)), ""),
        ("My Tool", format!("{}/mytool.exe", slash(&bin)), r#"-a \"b c\""#),
        ("Gone", format!("{}/gone.exe", slash(&bin)), ""),
        ("Hidden", format!("{}/hidden.exe", slash(&bin)), ""),
    ];
    let mut ini = String::from("[General]\r\ngameName=Skyrim Special Edition\r\nselected_profile=@ByteArray(Main)\r\n\r\n[customExecutables]\r\n");
    for (i, (title, binary, args)) in exes.iter().enumerate() {
        let n = i + 1;
        ini += &format!("{n}\\arguments={args}\r\n{n}\\binary={binary}\r\n{n}\\hide={}\r\n{n}\\title={title}\r\n{n}\\workingDirectory={}\r\n", *title == "Hidden", if *title == "My Tool" { slash(&bin) } else { String::new() });
    }
    ini += &format!("size={}\r\n", exes.len());
    write(&mo2.join("ModOrganizer.ini"), ini);
    write(&mo2.join("mods/A/A.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&mo2.join("mods/A/textures/a.dds.mohidden"), "hidden by MO2");
    write(&mo2.join("mods/A/old.mohidden/x.nif"), "a hidden folder");
    write(&mo2.join("mods/A/meta.ini"), "[General]\r\nmodid=123\r\nversion=1.0\r\nignoredVersion=2.0\r\n");
    write(&mo2.join("mods/BodySlide/CalienteTools/BodySlide/BodySlide x64.exe"), "exe");
    write(&mo2.join("mods/BodySlide Output/meshes/body.nif"), "built");
    write(&mo2.join("mods/Tool Stuff/stuff.txt"), "made by My Tool");
    write(&mo2.join("mods/C/meshes/c.nif"), "C");
    write(&mo2.join("profiles/Main/modlist.txt"), "+Tool Stuff\r\n+BodySlide Output\r\n+BodySlide\r\n+A\r\n");
    write(&mo2.join("profiles/Main/plugins.txt"), "*A.esp\r\n");
    write(&mo2.join("profiles/Main/settings.ini"), "[General]\r\nLocalSaves=true\r\nLocalSettings=true\r\n\r\n[custom_overwrites]\r\nMy%20Tool=Tool Stuff\r\n");
    write(&mo2.join("profiles/Main/skyrim.ini"), "[General]\r\nsMain=1\r\n");
    write(&mo2.join("profiles/Main/skyrimprefs.ini"), "[Display]\r\n");
    write(&mo2.join("profiles/Main/saves/Save1.ess"), "save");
    write(&mo2.join("profiles/Main/saves/Save1.skse"), "co-save");
    write(&mo2.join("profiles/Alt/modlist.txt"), "+C\r\n");
    let mut inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    inst.config.ini_path = Some(my.clone());
    let at = |p: &str| Instance { root: inst.root.clone(), config: InstanceConfig { profile: p.into(), ..inst.config.clone() } };

    // A dry run: every profile, mods and tools listed with the first.
    let dry = import::import_mo2_all(&inst, &mo2, true).unwrap();
    assert_eq!(dry.iter().map(|r| format!("{}>{}", r.profile, r.into)).collect::<Vec<_>>(), ["Main>Main", "Alt>Alt"]);
    assert_eq!((dry[0].mods.len(), dry[1].mods.len(), dry[1].existing.len()), (5, 0, 5));
    assert_eq!(dry[0].tools, ["SSEEdit", "BodySlide x64", "My Tool"], "not the game, SKSE, a missing or a hidden one");
    assert!(dry[0].warnings.iter().any(|w| w.contains("Gone")), "{:?}", dry[0].warnings);
    assert_eq!(dry[0].outputs, ["BodySlide Output (bodyslide)", "Tool Stuff (my-tool)"]);
    assert_eq!((dry[0].inis, dry[0].saves, dry[1].inis), (2, 2, 0));
    assert_eq!(inst.profiles().unwrap(), ["default"], "nothing made");
    assert!(tools::list(&inst).unwrap().is_empty() && !my.exists());

    let done = import::import_mo2_all(&inst, &mo2, false).unwrap();
    assert_eq!(inst.profiles().unwrap(), ["Alt", "default", "Main"]);
    assert_eq!((order(&at("Main")), order(&at("Alt"))), ("A+ BodySlide+ BodySlide Output+ Tool Stuff+ C".into(), "C+ A BodySlide BodySlide Output Tool Stuff".into()));
    assert!(order(&inst).is_empty(), "the active profile stays as it was");
    let main = at("Main").profile().unwrap();
    assert!(main.local_inis && main.local_saves && !at("Alt").profile().unwrap().local_saves, "MO2's profile options");
    assert_eq!(fs::read_to_string(inst.root.join("profiles/Main/ini/Skyrim.ini")).unwrap(), "[General]\r\nsMain=1\r\n");
    assert!(inst.root.join("profiles/Main/ini/SkyrimPrefs.ini").is_file() && my.join("Saves/Main/Save1.skse").is_file());

    // Hidden files stay hidden, under their own names; the ignored version is skipped by update checks.
    assert_eq!((done[0].hidden, inst.mod_files("A").unwrap()), (2, vec!["A.esp".to_string()]));
    let meta = inst.mod_meta("A");
    assert_eq!(meta.hidden, ["old/x.nif", "textures/a.dds"]);
    assert!(inst.mod_dir("A").join(".rmm-hidden/textures/a.dds").is_file());
    assert_eq!(meta.skip_version.as_deref(), Some("2.0"));
    let update = |latest: &str| nexus::Update { id: "A".into(), name: "A".into(), mod_id: 123, installed: Some("1.0".into()), latest: Some(latest.into()), file_id: 9, file_name: "A".into(), url: String::new() };
    assert!(!nexus::offered(&update("2.0"), &meta) && nexus::offered(&update("2.1"), &meta));

    // MO2's programs are tools: the presets rmm has, the rest as MO2 ran them.
    let t = tools::list(&inst).unwrap();
    assert_eq!(t.keys().collect::<Vec<_>>(), ["bodyslide", "my-tool", "xedit"]);
    assert!(t["xedit"].edit_in_place && t["xedit"].exe == bin.join("SSEEdit.exe"));
    assert!(slash(&t["bodyslide"].exe).ends_with("game/Data/CalienteTools/BodySlide/BodySlide x64.exe"), "run from the deployed Data folder");
    assert_eq!((t["my-tool"].args.clone(), t["my-tool"].output_mod.as_deref()), (vec!["-a".to_string(), "b c".into()], Some("Tool Stuff")));
    assert_eq!((inst.mod_meta("BodySlide Output").source.as_deref(), inst.mod_meta("Tool Stuff").source.as_deref()), (Some("tool:bodyslide"), Some("tool:my-tool")));

    // BodySlide's next run replaces the output MO2 made, in its place.
    deploy::deploy(&at("Main"), false).unwrap();
    let vars = BTreeMap::from([("groups".to_string(), "CBBE".to_string())]);
    assert_eq!(tools::run(&at("Main"), "bodyslide", &vars, true, None).unwrap().previous.as_deref(), Some("BodySlide Output"));

    // Again: nothing new.
    let again = import::import_mo2_all(&inst, &mo2, false).unwrap();
    assert!(again.iter().all(|r| r.mods.is_empty() && r.tools.is_empty() && r.outputs.is_empty() && r.saves == 0 && r.inis == 0), "{again:?}");
    assert_eq!(tools::list(&inst).unwrap().len(), 3);
}

#[test]
fn imports_a_vortex_backup() {
    isolate();
    use rmm_core::import;
    let tmp = tempfile::tempdir().unwrap();
    let (game, staging, local) = (tmp.path().join("game"), tmp.path().join("staging"), tmp.path().join("local"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&staging.join("Alpha-1-1-0/Alpha.esp"), plugin_bytes(&["Skyrim.esm"], false));
    write(&staging.join("Alpha-1-1-0/textures/x.dds"), "alpha");
    write(&staging.join("Beta-2/textures/x.dds"), "beta");
    write(&staging.join("Gamma-3/d3d11.dll"), "enb");
    write(&local.join("plugins.txt"), "# comment\n*Alpha.esp\n");
    let state = serde_json::json!({
        "persistent": {
            "mods": { "skyrimse": {
                "m1": { "state": "installed", "type": "", "installationPath": "Alpha-1-1-0",
                    "attributes": { "name": "Alpha", "logicalFileName": "Alpha", "installTime": "2024-01-01T00:00:00Z", "modId": 11, "fileId": "22", "version": "1.0",
                        "fileName": "Alpha-1-1-0.zip", "installerChoices": { "type": "fomod", "options": [{ "name": "Main", "groups": [{ "name": "G", "choices": [{ "name": "Opt", "idx": 0 }] }] }] } },
                    "rules": [{ "type": "after", "reference": { "id": "m2" } }] },
                "m2": { "state": "installed", "type": "", "installationPath": "Beta-2",
                    "attributes": { "name": "Beta", "installTime": "2024-01-02T00:00:00Z" },
                    "rules": [{ "type": "requires", "reference": { "logicalFileName": "Gamma" } }, { "type": "before", "reference": { "id": "m3" }, "ignored": true }] },
                "m3": { "state": "installed", "type": "dinput", "installationPath": "Gamma-3", "attributes": { "logicalFileName": "Gamma", "installTime": "2024-01-03T00:00:00Z" } },
                "m4": { "state": "downloaded", "attributes": { "name": "Not installed" } },
                "c1": { "state": "installed", "type": "collection", "installationPath": "coll", "attributes": { "name": "A collection" } }
            } },
            "profiles": {
                "p1": { "id": "p1", "gameId": "skyrimse", "name": "Default", "modState": { "m1": { "enabled": true }, "m2": { "enabled": true } } },
                "p2": { "id": "p2", "gameId": "fallout4", "name": "Other" }
            }
        },
        "settings": { "mods": { "installPath": { "skyrimse": staging.display().to_string() } }, "profiles": { "lastActiveProfile": { "skyrimse": "p1" } } }
    });
    let backup = tmp.path().join("startup.json");
    write(&backup, state.to_string());
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &local).unwrap();

    let dry = import::import_vortex(&inst, &backup, None, None, true).unwrap();
    assert_eq!((dry.profile.as_str(), dry.mods.join(" "), dry.rules), ("Default", "Alpha Beta Gamma".into(), 2));

    // The same state in Vortex's live LevelDB: one key per leaf, path parts joined by ###.
    fn leaves(v: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<(String, String)>) {
        match v.as_object() {
            Some(m) => {
                for (k, v) in m {
                    path.push(k.clone());
                    leaves(v, path, out);
                    path.pop();
                }
            }
            None => out.push((path.join("###"), v.to_string())),
        }
    }
    let db_dir = tmp.path().join("state.v2");
    {
        let mut db = rusty_leveldb::DB::open(&db_dir, rusty_leveldb::Options { create_if_missing: true, ..Default::default() }).unwrap();
        let mut kv = Vec::new();
        leaves(&state, &mut Vec::new(), &mut kv);
        for (k, v) in kv {
            db.put(k.as_bytes(), v.as_bytes()).unwrap();
        }
        db.flush().unwrap();
    }
    let live = import::import_vortex(&inst, &db_dir, None, None, true).unwrap();
    assert_eq!((live.profile, live.mods, live.rules), (dry.profile.clone(), dry.mods.clone(), dry.rules), "LevelDB reads like the backup");
    assert!(!inst.root.join(".staging/vortex-state").exists(), "the copy is cleaned up");
    let r = import::import_vortex(&inst, &backup, None, None, false).unwrap();
    assert!(r.warnings.is_empty(), "{r:?}");
    assert_eq!(order(&inst), "Beta+ Alpha+ Gamma", "Alpha loads after Beta by its rule");
    assert_eq!(inst.mod_files("Gamma").unwrap(), ["root/d3d11.dll"]);
    let meta = inst.mod_meta("Alpha");
    assert_eq!(meta.nexus.map(|n| (n.mod_id, n.file_id)), Some((11, Some(22))));
    assert_eq!(meta.fomod.unwrap().options[0].groups[0].choices[0].name, "Opt");
    let rules = rmm_core::rules::rules(&inst).unwrap();
    let rule = |id: &str| rules.iter().find(|r| r.id == id).unwrap().clone();
    assert_eq!((rule("Alpha").after, rule("Beta").requires), (vec!["Beta".to_string()], vec!["Gamma".to_string()]));
    assert_eq!(inst.profile().unwrap().plugins.iter().find(|e| e.name == "Alpha.esp").map(|e| e.enabled), Some(true));
    assert!(import::import_vortex(&inst, &backup, None, Some("Other"), true).is_err(), "a Fallout 4 profile");
}

#[test]
fn downloads_list_and_delete() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let dl = inst.root.join("downloads");
    fs::create_dir_all(&dl).unwrap();
    for name in ["A-1.zip", "B-2.zip"] {
        let mut z = zip::ZipWriter::new(fs::File::create(dl.join(name)).unwrap());
        z.start_file("textures/x.dds", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(name.as_bytes()).unwrap();
        z.finish().unwrap();
    }
    write(&dl.join("C.zip.part"), "still downloading");
    let a = install::install(&inst, &dl.join("A-1.zip"), None, true, None).unwrap().id;

    let mut list: Vec<(String, Vec<String>)> = install::downloads(&inst).unwrap().into_iter().map(|d| (d.name, d.mods)).collect();
    list.sort();
    assert_eq!(list, [("A-1.zip".to_string(), vec![a]), ("B-2.zip".to_string(), vec![])]);
    install::delete_download(&inst, "B-2.zip").unwrap();
    assert!(!dl.join("B-2.zip").exists());
    assert!(install::delete_download(&inst, "../instance.toml").is_err());
}

#[test]
fn lockfile_round_trip_keeps_user_mods() {
    isolate();
    use rmm_core::lockfile;
    let tmp = tempfile::tempdir().unwrap();
    let (game, dl) = (tmp.path().join("game"), tmp.path().join("downloads"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let zip = |name: &str, files: &[(&str, &[u8])]| {
        fs::create_dir_all(&dl).unwrap();
        let mut z = zip::ZipWriter::new(fs::File::create(dl.join(name)).unwrap());
        for (f, b) in files {
            z.start_file(*f, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(b).unwrap();
        }
        z.finish().unwrap();
    };
    zip("A-1.zip", &[("A.esp", &plugin_bytes(&["Skyrim.esm"], false)), ("textures/a.dds", b"A")]);
    zip("B-1.zip", &[("textures/a.dds", b"B")]);

    let one = Instance::create(&tmp.path().join("one"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&one, &dl.join("A-1.zip"), Some("Alpha"), true, None).unwrap();
    ops::add_separator(&one, "Textures", None).unwrap();
    install::install(&one, &dl.join("B-1.zip"), None, false, None).unwrap();
    ops::set_color(&one, "Alpha", "#112233").unwrap();
    assert!(one.mod_meta("Alpha").sha256.is_some_and(|h| h.len() == 64));
    let mut lock = lockfile::export(&one).unwrap();
    lock.mods.push(lockfile::LockedMod {
        id: "Gone".into(),
        enabled: true,
        archive: Some("Gone-5-6.zip".into()),
        nexus: Some(rmm_core::instance::NexusRef { mod_id: 5, file_id: Some(6), version: None, category: None }),
        ..Default::default()
    });
    let path = tmp.path().join("modlist.lock.toml");
    fs::write(&path, toml::to_string_pretty(&lock).unwrap()).unwrap();
    let lock = lockfile::read(&path).unwrap();
    assert_eq!(lock.mods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["Alpha", "sep:Textures", "B-1", "Gone"]);
    assert_eq!(lock.mods[0].archive.as_deref(), Some("A-1.zip"));

    // Another machine, where the user has a mod of their own.
    let two = Instance::create(&tmp.path().join("two"), Game::SkyrimSe, &game, &tmp.path().join("local2")).unwrap();
    write(&tmp.path().join("Mine/textures/m.dds"), "mine");
    install::install(&two, &tmp.path().join("Mine"), None, true, None).unwrap();
    let dry = lockfile::apply(&two, &lock, std::slice::from_ref(&dl), true).unwrap();
    assert_eq!((dry.install.join(" "), dry.kept.join(" ")), ("Alpha B-1".into(), "Mine".into()));
    assert_eq!(dry.missing[0].url.as_deref(), Some("https://www.nexusmods.com/skyrimspecialedition/mods/5?tab=files&file_id=6"));
    assert!(!two.mod_dir("Alpha").exists());

    let r = lockfile::apply(&two, &lock, std::slice::from_ref(&dl), false).unwrap();
    assert!(r.warnings.is_empty(), "{r:?}");
    assert_eq!(order(&two), "Mine+ Alpha+ sep:Textures B-1");
    assert_eq!(two.profile().unwrap().colors["Alpha"], "#112233");
    let again = lockfile::apply(&two, &lock, &[dl], true).unwrap();
    assert_eq!((again.ok, again.install.len(), again.differs.len()), (2, 0, 0), "same files as on the first machine");

    // A curated list moves on: the user's mod stays after the mod it followed.
    ops::move_mods(&two, &["Mine".to_string()], 1).unwrap();
    let mut next = lock.clone();
    next.mods.swap(0, 2); // B-1, sep:Textures, Alpha
    lockfile::apply(&two, &next, &[], false).unwrap();
    assert_eq!(order(&two), "B-1 sep:Textures Alpha+ Mine+");
}

/// A game folder whose exe says `version` and with the script extender's runtime for it.
fn versioned_game(dir: &Path, version: [u32; 3]) {
    let mut exe = b"MZ".to_vec();
    exe.extend([0xBD, 0x04, 0xEF, 0xFE, 0, 0, 1, 0]);
    exe.extend(((version[0] << 16) | version[1]).to_le_bytes());
    exe.extend((version[2] << 16).to_le_bytes());
    write(&dir.join("SkyrimSE.exe"), exe);
    write(&dir.join(format!("skse64_{}_{}_{}.dll", version[0], version[1], version[2])), "");
    write(&dir.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
}

/// Steam's updates are held by a read-only manifest (a copy kept) and let go again; playing
/// without mods purges and starts the game's own exe.
#[test]
fn steam_updates_and_playing_without_mods() {
    isolate();
    use rmm_core::{deploy, health, launch};
    let tmp = tempfile::tempdir().unwrap();
    let steamapps = tmp.path().join("lib/steamapps");
    let game = steamapps.join("common/Skyrim Special Edition");
    versioned_game(&game, [1, 6, 1170]);
    let manifest = steamapps.join("appmanifest_489830.acf");
    write(&manifest, "\"AppState\" { \"appid\" \"489830\" }");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();

    let st = launch::steam_updates(&inst);
    assert_eq!((st.manifest.as_deref(), st.held), (Some(manifest.as_path()), false));
    let st = launch::hold_steam_updates(&inst, true).unwrap();
    assert!(st.held && fs::metadata(&manifest).unwrap().permissions().readonly());
    assert_eq!(fs::read_to_string(st.backup.unwrap()).unwrap(), fs::read_to_string(&manifest).unwrap());
    assert!(health::check(&inst).unwrap().iter().any(|i| i.kind == "steam_updates_held"));
    assert!(!launch::hold_steam_updates(&inst, false).unwrap().held);
    assert!(!fs::metadata(&manifest).unwrap().permissions().readonly());
    assert!(!health::check(&inst).unwrap().iter().any(|i| i.kind == "steam_updates_held"));

    write(&tmp.path().join("src/Mod/textures/a.dds"), "a");
    install::install(&inst, &tmp.path().join("src/Mod"), None, true, None).unwrap();
    deploy::deploy(&inst, false).unwrap();
    assert!(game.join("Data/textures/a.dds").is_file());
    let r = launch::launch_vanilla(&inst, true).unwrap();
    assert_eq!((r.exe.file_name().unwrap().to_str(), r.script_extender.as_deref(), r.game_version.as_str()), (Some("SkyrimSE.exe"), None, "1.6.1170.0"));
    assert!(r.deploy.remove.iter().any(|f| f.to_lowercase().replace('\\', "/").ends_with("textures/a.dds")), "{:?}", r.deploy.remove);
    assert!(game.join("Data/textures/a.dds").is_file(), "a dry run leaves the game folder");

    // A game outside a Steam library.
    let other = Instance::create(&tmp.path().join("inst2"), Game::SkyrimSe, &tmp.path().join("lib/steamapps/common/Skyrim Special Edition"), &tmp.path().join("local2")).unwrap();
    fs::remove_file(&manifest).unwrap();
    assert!(launch::steam_updates(&other).manifest.is_none());
    assert!(format!("{:#}", launch::hold_steam_updates(&other, true).unwrap_err()).contains("appmanifest_489830.acf"));
}

/// A pack brings the mods no archive does, with the notes; apply warns about another game
/// version; diff and the readable list say what's what.
#[test]
fn lockfile_pack_diff_and_markdown() {
    use rmm_core::lockfile;
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (game, game2, dl) = (tmp.path().join("game"), tmp.path().join("game2"), tmp.path().join("dl"));
    versioned_game(&game, [1, 6, 1170]);
    versioned_game(&game2, [1, 6, 640]);
    fs::create_dir_all(&dl).unwrap();
    let mut z = zip::ZipWriter::new(fs::File::create(dl.join("A-1.zip")).unwrap());
    z.start_file("A.esp", zip::write::SimpleFileOptions::default()).unwrap();
    z.write_all(&plugin_bytes(&["Skyrim.esm"], false)).unwrap();
    z.finish().unwrap();
    write(&tmp.path().join("Patch/Patch.esp"), plugin_bytes(&["Skyrim.esm", "A.esp"], false));

    let one = Instance::create(&tmp.path().join("one"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&one, &dl.join("A-1.zip"), Some("Alpha"), true, None).unwrap();
    ops::add_separator(&one, "Patches", None).unwrap();
    install::install(&one, &tmp.path().join("Patch"), None, true, None).unwrap();
    ops::set_mod_note(&one, "Alpha", "Read the FAQ first.").unwrap();
    let pack = tmp.path().join("share.zip");
    let w = lockfile::write(&one, Some(&pack), lockfile::Format::Pack).unwrap();
    assert_eq!(w.packed, ["Patch"], "Alpha comes from its archive");
    let lock = lockfile::read(&pack).unwrap();
    assert_eq!(lock.env.game_version.as_deref(), Some("1.6.1170.0"));
    assert_eq!(lock.env.script_extender, ["skse64_1_6_1170.dll"]);

    let md = lockfile::markdown(&lock);
    for want in ["# Skyrim Special Edition mod list", "Made with game version 1.6.1170.0", "- [x] Alpha (`A-1.zip`)", "  > Read the FAQ first.", "## Patches", "- [x] Patch", "1. A.esp", "2. Patch.esp"] {
        assert!(md.contains(want), "{want} in\n{md}");
    }

    // A friend on an older game version.
    let two = Instance::create(&tmp.path().join("two"), Game::SkyrimSe, &game2, &tmp.path().join("local2")).unwrap();
    let r = lockfile::apply(&two, &lock, std::slice::from_ref(&dl), false).unwrap();
    assert_eq!((r.install.join(" "), r.missing.len()), ("Alpha Patch".into(), 0));
    assert!(r.warnings.iter().any(|w| w.contains("game version 1.6.1170.0, this game is 1.6.640.0")), "{:?}", r.warnings);
    assert!(r.warnings.iter().any(|w| w.contains("skse64_1_6_640.dll")), "{:?}", r.warnings);
    assert!(two.mod_dir("Patch").join("Patch.esp").is_file());
    assert_eq!(two.mod_meta("Patch").source, None, "not the unpacked folder, which is gone");
    assert!(!two.root.join(".pack").exists());
    assert_eq!(two.mod_meta("Alpha").notes.as_deref(), Some("Read the FAQ first."));
    assert_eq!(order(&two), "Alpha+ sep:Patches Patch+");
    assert!(lockfile::diff(&lock, &lockfile::export(&two).unwrap()).added.is_empty());

    // Then they change things.
    ops::set_mods_enabled(&two, &["Alpha".to_string()], false).unwrap();
    ops::move_mods(&two, &["Patch".to_string()], 0).unwrap();
    let d = lockfile::diff(&lock, &lockfile::export(&two).unwrap());
    assert_eq!(d.versions, ["game version: 1.6.1170.0 → 1.6.640.0", "script extender: skse64_1_6_1170.dll → skse64_1_6_640.dll"]);
    assert_eq!((d.turned_off.join(" "), d.moved.len(), d.added.len(), d.removed.len(), d.changed.len()), ("Alpha".into(), 1, 0, 0, 0));
    assert_eq!(d.plugins_off, ["A.esp"]);
}

type Hits = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>;

/// A stand-in for the Nexus API on 127.0.0.1. `routes` gets the server's base URL and returns
/// the answer for each path and query; /v1 requests without the test key get a 401.
fn mock_nexus(routes: impl FnOnce(&str) -> std::collections::HashMap<String, Vec<u8>>) -> (String, Hits) {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = routes(&base);
    let hits: Hits = Default::default();
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut keyed = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                keyed |= line.to_lowercase().starts_with("apikey: test-key");
            }
            *counter.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
            let (status, body) = match routes.get(&path) {
                _ if path.starts_with("/v1") && !keyed => ("401 Unauthorized", br#"{"message":"Please provide a valid API Key"}"#.to_vec()),
                Some(b) => ("200 OK", b.clone()),
                None => ("404 Not Found", b"{}".to_vec()),
            };
            let mut out = stream;
            let _ = write!(out, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = out.write_all(&body);
        }
    });
    (base, hits)
}

/// Downloads are looked up on Nexus by MD5, many at once; a mod made from one gets its ids. A
/// download whose server can't resume starts over, and leftovers bigger than the file go.
#[test]
fn identifies_downloads_and_downloads_again() {
    isolate();
    use rmm_core::nexus;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let dl = inst.downloads_dir();
    write(&dl.join("SkyUI.7z"), "abc"); // MD5 900150983cd24fb0d6963f7d28e17f72
    write(&dl.join("Mine.zip"), "xyz");
    write(&tmp.path().join("src/UI/ui.txt"), "ui");
    install::install(&inst, &tmp.path().join("src/UI"), None, true, None).unwrap();
    let mut meta = inst.mod_meta("UI");
    meta.source = Some(dl.join("SkyUI.7z").display().to_string());
    inst.write_mod_meta("UI", &meta).unwrap();

    let json = |v: serde_json::Value| v.to_string().into_bytes();
    let d = "/v1/games/skyrimspecialedition/mods";
    let (base, hits) = mock_nexus(|base| {
        std::collections::HashMap::from([
            (format!("{d}/md5_search/900150983cd24fb0d6963f7d28e17f72.json"), json(serde_json::json!([{
                "mod": { "mod_id": 12604, "name": "SkyUI", "category_id": 42 },
                "file_details": { "file_id": 35407, "version": "5.2SE", "md5": "900150983cd24fb0d6963f7d28e17f72" }
            }]))),
            ("/v1/users/validate.json".to_string(), json(serde_json::json!({ "user_id": 1, "name": "tester", "is_premium": true }))),
            (format!("{d}/7/files/8.json"), json(serde_json::json!({ "file_id": 8, "file_name": "Big.zip", "size_in_bytes": 10 }))),
            (format!("{d}/7/files/8/download_link.json"), json(serde_json::json!([{ "URI": format!("{base}/cdn/big.zip") }]))),
            ("/cdn/big.zip".to_string(), b"0123456789".to_vec()),
        ])
    });
    let mut client = nexus::Client::new("test-key").unwrap();
    client.base = base;

    let r = nexus::identify(&inst, &client, &[]).unwrap();
    assert_eq!(r.found.len(), 1, "{r:?}");
    assert_eq!((r.found[0].0.as_str(), r.found[0].1.name.as_deref(), r.found[0].1.nexus.file_id), ("SkyUI.7z", Some("SkyUI"), Some(35407)));
    assert_eq!((r.not_found.clone(), r.mods.clone()), (vec!["Mine.zip".to_string()], vec!["UI".to_string()]));
    assert_eq!(inst.mod_meta("UI").nexus.map(|n| n.mod_id), Some(12604));
    let listed = install::downloads(&inst).unwrap();
    assert_eq!(listed.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().len(), 2, "the store isn't listed");
    assert!(listed.iter().any(|d| d.name == "SkyUI.7z" && d.nexus.as_ref().is_some_and(|n| n.nexus.version.as_deref() == Some("5.2SE"))));
    let again = nexus::identify(&inst, &client, &[]).unwrap();
    assert_eq!((again.found.len(), again.not_found.len()), (0, 1), "only what isn't identified yet");

    // The stand-in CDN can't resume, so the left-over part is dropped and the file comes whole.
    write(&dl.join("Big.zip.part"), "0123");
    let link = nexus::premium_link(&inst, 7, 8);
    assert_eq!(fs::read(nexus::download(&inst, &client, &link).unwrap()).unwrap(), b"0123456789");
    fs::remove_file(dl.join("Big.zip")).unwrap();
    write(&dl.join("Big.zip.part"), "far more than ten bytes");
    assert_eq!(fs::read(nexus::download(&inst, &client, &link).unwrap()).unwrap(), b"0123456789");
    assert!(!dl.join("Big.zip.part").exists());
    assert_eq!(hits.lock().unwrap()["/cdn/big.zip"], 3, "once to try to resume, once whole, once whole");
}

/// Hiding a mod's files takes them out of deploys, conflicts and the plugin list, and a reinstall
/// keeps them hidden; files move from Overwrite into a mod, a clash only with replace (the old
/// copy to trash), each one undo step, and Problems mentions what's left in Overwrite.
#[test]
fn hides_and_moves_files() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let src = tmp.path().join("src");
    write(&src.join("A/textures/a.dds"), "from A");
    write(&src.join("A/A.esp"), plugin_bytes(&[], false));
    write(&src.join("B/textures/a.dds"), "from B");
    write(&src.join("B/textures/b.dds"), "b");
    write(&src.join("B/B.esp"), plugin_bytes(&[], false));
    for m in ["A", "B"] {
        install::install(&inst, &src.join(m), None, true, None).unwrap();
    }
    let winner = || {
        deploy::deploy(&inst, false).unwrap();
        fs::read_to_string(game.join("Data/textures/a.dds")).unwrap()
    };
    assert_eq!(winner(), "from B");

    assert_eq!(install::hide_files(&inst, "B", &["Textures\\A.dds".into()], true).unwrap(), ["textures/a.dds"]);
    assert_eq!(winner(), "from A", "the hidden file is out of the deploy");
    assert_eq!(inst.mod_files("B").unwrap(), ["B.esp", "textures/b.dds"]);
    assert!(conflicts::conflicts(&inst, &inst.profile().unwrap()).unwrap().iter().all(|c| c.overwrites.is_empty() && c.overwritten_by.is_empty()));
    install::reinstall(&inst, "B", None).unwrap();
    assert_eq!((inst.mod_files("B").unwrap(), inst.mod_meta("B").hidden), (vec!["B.esp".to_string(), "textures/b.dds".to_string()], vec!["textures/a.dds".to_string()]), "a reinstall hides it again");
    install::hide_files(&inst, "B", &["textures/a.dds".into()], false).unwrap();
    assert_eq!(winner(), "from B");
    assert!(inst.mod_meta("B").hidden.is_empty());
    // A hidden plugin is out of the plugin list: MO2's optional ESP.
    install::hide_files(&inst, "A", &["A.esp".into()], true).unwrap();
    let listed = || plugins::list(&inst, &inst.profile().unwrap()).unwrap().iter().any(|p| p.name == "A.esp");
    assert!(!listed());
    // Hiding is an undo step: Ctrl+Z shows it again, rather than undoing the step before.
    assert_eq!(inst.undo().unwrap().as_deref(), Some("hide A.esp in A"));
    assert!(listed() && inst.mod_meta("A").hidden.is_empty());
    inst.redo().unwrap();
    assert!(!listed() && inst.mod_meta("A").hidden == ["A.esp"]);
    assert!(install::hide_files(&inst, "A", &["nope.esp".into()], true).is_err());

    let over = inst.mod_dir(rmm_core::instance::OVERWRITE);
    write(&over.join("MCM/Settings/a.ini"), "mine");
    write(&over.join("SKSE/Plugins/x.ini"), "x");
    let issue = || health::check(&inst).unwrap().into_iter().find(|i| i.kind == "overwrite_files").map(|i| i.message);
    assert!(issue().unwrap().starts_with("2 files belong to no mod"));
    assert_eq!(install::move_files(&inst, rmm_core::instance::OVERWRITE, &["MCM".into()], "A", false).unwrap(), ["MCM/Settings/a.ini"]);
    assert!(inst.mod_files("A").unwrap().contains(&"MCM/Settings/a.ini".to_string()) && !over.join("MCM").exists(), "emptied folders go");
    assert!(issue().unwrap().starts_with("1 file belongs"));
    write(&over.join("MCM/Settings/a.ini"), "newer");
    let clash = install::move_files(&inst, rmm_core::instance::OVERWRITE, &["MCM/Settings/a.ini".into()], "A", false);
    assert!(format!("{:#}", clash.unwrap_err()).contains("has 1 of these already"));
    install::move_files(&inst, rmm_core::instance::OVERWRITE, &["MCM/Settings/a.ini".into()], "A", true).unwrap();
    assert_eq!(fs::read_to_string(inst.mod_dir("A").join("MCM/Settings/a.ini")).unwrap(), "newer");
    // Undo puts both back: the target's replaced copy and the file in Overwrite.
    assert_eq!(inst.undo().unwrap().as_deref(), Some("move a.ini from Overwrite to A"));
    assert_eq!(fs::read_to_string(inst.mod_dir("A").join("MCM/Settings/a.ini")).unwrap(), "mine");
    assert_eq!(fs::read_to_string(over.join("MCM/Settings/a.ini")).unwrap(), "newer");
    inst.redo().unwrap();
    assert_eq!(fs::read_to_string(inst.mod_dir("A").join("MCM/Settings/a.ini")).unwrap(), "newer");
    assert!(!over.join("MCM/Settings/a.ini").exists());
    install::move_files(&inst, rmm_core::instance::OVERWRITE, &["SKSE".into()], "B", false).unwrap();
    assert!(issue().is_none());
}

/// Creation Club content becomes mods, on at the top of every profile and kept through undo;
/// deploy links it back, turning one off leaves it out, and restoring puts it back in Data.
#[test]
fn adopts_creation_club_content() {
    isolate();
    use rmm_core::import;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&game.join("Data/ccBGSSSE001-Fish.esm"), plugin_bytes(&["Skyrim.esm"], true));
    write(&game.join("Data/ccBGSSSE001-Fish.bsa"), "archive");
    write(&game.join("Data/ccBGSSSE025-AdvDSGS.esm"), plugin_bytes(&["Skyrim.esm"], true));
    write(&game.join("Skyrim.ccc"), "ccBGSSSE001-Fish.esm\r\nccQDRSSE001-SurvivalMode.esl\r\nccBGSSSE025-AdvDSGS.esm\r\n");
    let mut inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    write(&tmp.path().join("src/Mine/Mine.esp"), plugin_bytes(&[], false));
    install::install(&inst, &tmp.path().join("src/Mine"), None, true, None).unwrap();
    inst.create_profile("Other", true).unwrap();

    let plan = import::adopt_creations(&inst, true).unwrap();
    assert_eq!((plan.mods.clone(), plan.files), (vec!["ccBGSSSE001-Fish.esm".to_string(), "ccBGSSSE025-AdvDSGS.esm".to_string()], 3), "listed ones that aren't there are skipped");
    assert!(game.join("Data/ccBGSSSE001-Fish.bsa").is_file(), "a dry run moves nothing");
    import::adopt_creations(&inst, false).unwrap();
    assert!(!game.join("Data/ccBGSSSE001-Fish.bsa").exists());
    assert_eq!(inst.mod_files("ccBGSSSE001-Fish").unwrap(), ["ccBGSSSE001-Fish.bsa", "ccBGSSSE001-Fish.esm"]);
    let order = |inst: &Instance| inst.profile().unwrap().mods.iter().map(|m| (m.id.clone(), m.enabled)).collect::<Vec<_>>();
    let want = vec![("ccBGSSSE001-Fish".to_string(), true), ("ccBGSSSE025-AdvDSGS".to_string(), true), ("Mine".to_string(), true)];
    assert_eq!(order(&inst), want);
    assert!(import::adopt_creations(&inst, true).unwrap().mods.is_empty(), "nothing left to adopt");
    deploy::deploy(&inst, false).unwrap();
    assert!(game.join("Data/ccBGSSSE001-Fish.bsa").is_file(), "deploy links it back");

    // Undo past the adoption (and Mine's install) keeps it; another profile has it on too.
    ops::set_mods_enabled(&inst, &["ccBGSSSE025-AdvDSGS".into()], false).unwrap();
    inst.undo().unwrap();
    inst.undo().unwrap();
    assert!(order(&inst).iter().any(|(id, on)| id == "ccBGSSSE001-Fish" && *on), "{:?}", order(&inst));
    inst.switch_profile("Other").unwrap();
    assert_eq!(&order(&inst)[..2], &want[..2]);
    inst.switch_profile("default").unwrap();
    ops::set_mods_enabled(&inst, &["ccBGSSSE001-Fish".into()], false).unwrap();
    deploy::deploy(&inst, false).unwrap();
    assert!(!game.join("Data/ccBGSSSE001-Fish.bsa").exists(), "turned off, it's out of the game");

    let back = import::restore_creations(&inst, false).unwrap();
    assert_eq!((back.mods.len(), back.files), (2, 3));
    assert!(game.join("Data/ccBGSSSE001-Fish.bsa").is_file() && game.join("Data/ccBGSSSE025-AdvDSGS.esm").is_file());
    assert_eq!(order(&inst), [("Mine".to_string(), false)], "(the second undo took back Mine's install)");
    assert!(!inst.mod_dir("ccBGSSSE001-Fish").exists());
}

/// Every plugin LOOT knows as dirty is cleaned with the QuickAutoClean tool, one run each, into its
/// own mod; one with only deleted navmeshes is left for xEdit by hand.
#[test]
fn cleans_dirty_plugins_in_one_job() {
    isolate();
    use rmm_core::tools::{self, ToolSpec};
    let tmp = tempfile::tempdir().unwrap();
    let (game, src, root) = (tmp.path().join("game"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let dirty = plugin_bytes(&["Skyrim.esm"], false);
    let clean = plugin_bytes(&[], false);
    write(&src.join("Dirty/Dirty.esp"), &dirty);
    write(&src.join("Nav/Nav.esp"), &dirty);
    write(&src.join("clean.esp"), &clean);
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    for m in ["Dirty", "Nav"] {
        install::install(&inst, &src.join(m), None, true, None).unwrap();
    }
    let crc = crc32(&dirty);
    write(
        &root.join("loot/masterlist.yaml"),
        format!(
            "plugins:\n  - name: 'Dirty.esp'\n    dirty:\n      - crc: 0x{crc:08X}\n        util: 'SSEEdit v4.1.5'\n        itm: 3\n        udr: 2\n  \
             - name: 'Nav.esp'\n    dirty:\n      - crc: 0x{crc:08X}\n        util: 'SSEEdit v4.1.5'\n        nav: 1\n"
        ),
    );
    write(&root.join("loot/prelude.yaml"), "common: []\n");
    deploy::deploy(&inst, false).unwrap();
    assert!(format!("{:#}", tools::clean_dirty(&inst, true).unwrap_err()).contains("xedit-qac"), "no QuickAutoClean tool yet");

    // A stand-in for QuickAutoClean: copies a clean plugin over the one it's given.
    write(&src.join("qac.bat"), "@echo off\r\ncopy /y \"%~4\" \"%~2\\%~3\" >nul\r\n");
    let spec = ToolSpec {
        exe: src.join("qac.bat"),
        args: Some(vec!["-qac".into(), "{data}".into(), "{plugin}".into(), src.join("clean.esp").display().to_string()]),
        rewrites: Some(vec!["{plugin}".into()]),
        output_mod: Some("Cleaned {plugin}".into()),
        ..Default::default()
    };
    tools::add(&inst, "qac", spec).unwrap();
    let names = |d: &[rmm_core::loot::Dirty]| d.iter().map(|d| d.plugin.clone()).collect::<Vec<_>>();
    let plan = tools::clean_dirty(&inst, true).unwrap();
    assert_eq!((plan.tool.as_str(), names(&plan.plugins), names(&plan.by_hand)), ("qac", vec!["Dirty.esp".to_string()], vec!["Nav.esp".to_string()]));
    assert_eq!((plan.plugins[0].itm, plan.plugins[0].deleted_references), (3, 2));

    let r = tools::clean_dirty(&inst, false).unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(r.mods, [("Dirty.esp".to_string(), Some("Cleaned Dirty.esp".to_string()))]);
    assert!(r.still_dirty.is_empty(), "the clean copy has another CRC");
    assert_eq!(fs::read(game.join("Data/Dirty.esp")).unwrap(), clean, "the clean copy is deployed");
    assert_eq!(fs::read(inst.mod_dir("Dirty").join("Dirty.esp")).unwrap(), dirty, "the original stays in its mod");
}

/// A desktop shortcut runs `rmm launch --profile <p>` on this instance; a dry run writes nothing.
#[cfg(windows)]
#[test]
fn makes_a_profile_shortcut() {
    isolate();
    use rmm_core::launch::shortcut;
    let tmp = tempfile::tempdir().unwrap();
    let (game, root, desk) = (tmp.path().join("game"), tmp.path().join("inst"), tmp.path().join("desk"));
    write(&game.join("SkyrimSE.exe"), "exe");
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    inst.create_profile("Pack", true).unwrap();
    let cli = tmp.path().join("rmm.exe");
    assert!(shortcut(&inst, "nope", &cli, Some(&desk), true).is_err());
    let plan = shortcut(&inst, "pack", &cli, Some(&desk), true).unwrap();
    assert_eq!(plan.path, desk.join("Skyrim Special Edition - Pack.lnk"));
    assert_eq!(plan.arguments, format!("-i \"{}\" launch --profile \"Pack\" --apply --pause", root.display()));
    assert!(!desk.exists(), "a dry run writes nothing");
    shortcut(&inst, "Pack", &cli, Some(&desk), false).unwrap();
    assert!(plan.path.is_file());
}

/// The panel's read-me's, settings and pictures come from the mod's files.
#[test]
fn mod_details_lists_docs_and_pictures() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let (game, src, root) = (tmp.path().join("game"), tmp.path().join("src/Docs"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    for f in ["Readme.TXT", "SKSE/Plugins/docs.ini", "Interface/Translations/docs_english.txt", "shot.JPG", "textures/a.dds", "meshes/b.nif"] {
        write(&src.join(f), "x");
    }
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &src, None, true, None).unwrap();
    let d = rmm_core::ops::mod_details(&inst, "Docs", 300).unwrap();
    assert_eq!(d["docs"], serde_json::json!(["Readme.TXT", "SKSE/Plugins/docs.ini"]));
    assert_eq!(d["images"], serde_json::json!(["shot.JPG"]));
}

/// A whole-setup tool's output records what it was made from; a new mod makes it stale (and goes
/// in before it), and a rebuild runs the tool again.
#[test]
fn rebuilds_stale_generated_output() {
    isolate();
    use rmm_core::tools::{self, ToolSpec};
    let tmp = tempfile::tempdir().unwrap();
    let (game, src, root) = (tmp.path().join("game"), tmp.path().join("src"), tmp.path().join("inst"));
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    for m in ["A", "B", "C"] {
        write(&src.join(m).join(format!("{m}.esp")), plugin_bytes(&["Skyrim.esm"], false));
    }
    let inst = Instance::create(&root, Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    for m in ["A", "B"] {
        install::install(&inst, &src.join(m), None, true, None).unwrap();
    }
    // A stand-in generator: writes a file into {output}.
    write(&src.join("gen.bat"), "@echo off\r\necho made> \"%~1\\out.txt\"\r\n");
    let spec = |exe: &str| ToolSpec { exe: src.join(exe), args: Some(vec!["{output}".into()]), ..Default::default() };
    tools::add(&inst, "synthesis", spec("gen.bat")).unwrap();
    tools::add(&inst, "mine", spec("gen.bat")).unwrap();
    let out = tools::run(&inst, "synthesis", &Default::default(), false, None).unwrap().mod_id.unwrap();
    let mine = tools::run(&inst, "mine", &Default::default(), false, None).unwrap().mod_id.unwrap();
    assert!(inst.mod_meta(&out).built_from.is_some_and(|b| b.mods.keys().eq(["A", "B"])), "generated output isn't an input");
    assert!(inst.mod_meta(&mine).built_from.is_none(), "only whole-setup tools keep track");
    assert!(tools::stale_outputs(&inst).unwrap().is_empty());
    assert!(tools::rebuild(&inst, true, None).unwrap().steps.is_empty());

    install::install(&inst, &src.join("C"), None, true, None).unwrap();
    let ids: Vec<String> = inst.profile().unwrap().mods.into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["A", "B", "C", out.as_str(), mine.as_str()], "a new mod goes in before generated output");
    let stale = tools::stale_outputs(&inst).unwrap();
    assert_eq!((stale.len(), stale[0].summary().as_str()), (1, "1 mod added (C)"));
    assert!(rmm_core::health::check(&inst).unwrap().iter().any(|i| i.kind == "stale_output" && i.subject == out));
    let plan = tools::rebuild(&inst, true, None).unwrap();
    assert_eq!(plan.steps, [("synthesis".to_string(), "1 mod added (C)".to_string())]);

    let r = tools::rebuild(&inst, false, None).unwrap();
    assert!(r.failed.is_none() && r.done.len() == 1 && r.done[0].1.is_some(), "{r:?}");
    assert!(tools::stale_outputs(&inst).unwrap().is_empty(), "rebuilt from the current setup");
}

/// A zip of (path, bytes).
fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
    for (name, data) in files {
        z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(data).unwrap();
    }
    z.finish().unwrap();
    bytes
}

/// A collection's newer revision updates its profile: a newer file in place (beside the old one
/// when another profile has that on), dropped mods turned off, new ones installed. The user's own
/// mods, and the collection's mods and plugins they turned off, stay as they are.
#[test]
fn collection_update_keeps_your_changes() {
    isolate();
    use rmm_core::{collection, nexus};
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let esp = plugin_bytes(&[], false);
    let json = |v: serde_json::Value| v.to_string().into_bytes();
    // A stand-in Nexus serving one revision; each mod's archive has <name>.esp and <name>-<file>.txt.
    let serve = |rev: u64, mods: &[(&str, u64, u64)], plugins: serde_json::Value| {
        let list: Vec<_> = mods.iter().map(|(n, id, file)| serde_json::json!({ "name": n, "optional": false, "phase": 0, "source": { "type": "nexus", "modId": id, "fileId": file }, "details": {} })).collect();
        let manifest = serde_json::json!({ "info": { "name": "Pack", "domainName": "skyrimspecialedition", "gameVersions": [] }, "mods": list, "modRules": [], "plugins": plugins });
        let (base, _) = mock_nexus(|base| {
            let link = format!("/v1/collections/pk/revisions/{rev}/download_link.json");
            let mut routes = std::collections::HashMap::from([
                ("/v1/users/validate.json".to_string(), json(serde_json::json!({ "user_id": 1, "name": "tester", "is_premium": true }))),
                ("/v2/graphql".to_string(), json(serde_json::json!({ "data": { "collectionRevision": { "revisionNumber": rev, "downloadLink": link, "collection": { "name": "Pack" }, "modFiles": [] } } }))),
                (link.clone(), json(serde_json::json!({ "download_links": [{ "URI": format!("{base}/cdn/bundle.zip") }] }))),
                ("/cdn/bundle.zip".to_string(), zip_of(&[("collection.json", manifest.to_string().as_bytes())])),
            ]);
            for (n, id, file) in mods {
                let d = format!("/v1/games/skyrimspecialedition/mods/{id}/files/{file}");
                routes.insert(format!("{d}.json"), json(serde_json::json!({ "file_id": file, "file_name": format!("{n}-{id}-{file}-0-1700000000.zip") })));
                routes.insert(format!("{d}/download_link.json"), json(serde_json::json!([{ "URI": format!("{base}/cdn/{file}.zip") }])));
                routes.insert(format!("/cdn/{file}.zip"), zip_of(&[(&format!("{n}.esp"), &esp), (&format!("{n}-{file}.txt"), b"x")]));
            }
            routes
        });
        let mut client = nexus::Client::new("test-key").unwrap();
        client.base = base;
        client
    };
    let mut inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let on = serde_json::json!([{ "name": "A.esp", "enabled": true }, { "name": "B.esp", "enabled": true }]);
    let first = serve(1, &[("A", 1, 11), ("B", 2, 21), ("C", 3, 31), ("E", 5, 51)], on);
    let link = "https://www.nexusmods.com/games/skyrimspecialedition/collections/pk";
    let r = collection::install(&mut inst, &first, link, None, false, Some("Pack"), false).unwrap();
    assert!(r.failed.is_empty() && r.update_from.is_none(), "{r:?}");
    assert_eq!(inst.profile().unwrap().collection.map(|c| (c.revision, c.mods)), Some((1, ["A", "B", "C", "E"].map(String::from).to_vec())));

    // Another profile uses E; here the user adds a mod, turns A off and B's plugin off.
    inst.switch_profile("default").unwrap();
    ops::set_mods_enabled(&inst, &["E".into()], true).unwrap();
    inst.switch_profile("Pack").unwrap();
    write(&tmp.path().join("src/Mine/Mine.esp"), plugin_bytes(&[], false));
    install::install(&inst, &tmp.path().join("src/Mine"), None, true, None).unwrap();
    ops::set_mods_enabled(&inst, &["A".into()], false).unwrap();
    ops::set_plugins_enabled(&inst, &["B.esp".into()], false).unwrap();

    let plugins2 = serde_json::json!([{ "name": "A.esp", "enabled": true }, { "name": "B.esp", "enabled": true }, { "name": "D.esp", "enabled": false }]);
    let second = serve(2, &[("A", 1, 11), ("B", 2, 22), ("D", 4, 41), ("E", 5, 52)], plugins2);
    let plan = collection::install(&mut inst, &second, "", None, false, None, true).unwrap();
    let strs = |v: &[String]| v.to_vec();
    assert_eq!(plan.update_from, Some(1), "an empty link is the profile's collection");
    assert_eq!((strs(&plan.installed), strs(&plan.updated), strs(&plan.install)), (vec!["A".into()], vec!["B".into(), "E".into()], vec!["D".into()]));
    assert_eq!((strs(&plan.kept_off), strs(&plan.turned_off), strs(&plan.yours)), (vec!["A".into()], vec!["C".into(), "E".into()], vec!["Mine".into()]));

    let r = collection::install(&mut inst, &second, "", None, false, None, false).unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    let p = inst.profile().unwrap();
    let mods: Vec<(&str, bool)> = p.mods.iter().map(|m| (m.id.as_str(), m.enabled)).collect();
    assert_eq!(mods, [("A", false), ("B", true), ("C", false), ("E", false), ("E (2)", true), ("Mine", true), ("D", true)]);
    assert!(inst.mod_files("B").unwrap().contains(&"B-22.txt".to_string()) && !inst.mod_files("B").unwrap().contains(&"B-21.txt".to_string()), "B is updated in place");
    assert!(inst.mod_files("E (2)").unwrap().contains(&"E-52.txt".to_string()) && inst.mod_files("E").unwrap().contains(&"E-51.txt".to_string()), "E's new file goes beside the old one");
    assert!(inst.mod_dir("C").is_dir(), "a dropped mod stays installed");
    let plugin = |n: &str| p.plugins.iter().find(|e| e.name == n).map(|e| e.enabled);
    assert_eq!((plugin("B.esp"), plugin("D.esp"), plugin("Mine.esp")), (Some(false), Some(false), Some(true)), "the user's plugin choice and the curator's new one");
    assert_eq!(p.collection.map(|c| (c.revision, c.mods)), Some((2, ["A", "B", "D", "E (2)"].map(String::from).to_vec())));

    let again = collection::install(&mut inst, &second, "", None, false, None, true).unwrap();
    assert_eq!((again.update_from, again.install.len(), again.updated.len(), again.turned_off.len()), (Some(2), 0, 0, 0), "{again:?}");
    inst.switch_profile("default").unwrap();
    assert_eq!(inst.profile().unwrap().mods.iter().find(|m| m.id == "E").map(|m| m.enabled), Some(true), "the other profile keeps its E");
}

/// A collection from a stand-in Nexus: a dry run, then an install into a new profile. A mod
/// already installed from the same file is turned on there, a "dinput" mod's non-Data files go
/// to the game folder (and stay there on reinstall), and the user's own profile is left alone.
#[test]
fn collection_into_a_new_profile() {
    isolate();
    use rmm_core::{collection, nexus};
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let source = |id: u64, file: u64, logical: &str| serde_json::json!({ "type": "nexus", "modId": id, "fileId": file, "logicalFilename": logical });
    let manifest = serde_json::json!({
        "info": { "name": "Test Pack", "domainName": "skyrimspecialedition", "gameVersions": [] },
        "mods": [
            { "name": "Have", "optional": false, "phase": 0, "source": source(1, 11, "Have"), "details": {} },
            { "name": "Enb Stuff", "optional": false, "phase": 1, "source": source(2, 21, "Enb"), "details": { "type": "dinput" }, "instructions": "Use the high preset." },
            { "name": "Optional One", "optional": true, "phase": 1, "source": source(3, 31, "Opt"), "details": {} },
        ],
        "modRules": [{ "type": "after", "source": { "logicalFileName": "Enb" }, "reference": { "logicalFileName": "Have" } }],
        "plugins": [{ "name": "Enb.esp", "enabled": false }],
    });
    let bundle = zip_of(&[("collection.json", manifest.to_string().as_bytes())]);
    let esp = plugin_bytes(&[], false);
    let enb = zip_of(&[("enbseries/a.fx", b"fx"), ("enblocal.ini", b"[GLOBAL]"), ("SKSE/Plugins/x.dll", b"dll"), ("Enb.esp", &esp), ("Enb.ini", b"[x]")]);
    let json = |v: serde_json::Value| v.to_string().into_bytes();
    let d = "/v1/games/skyrimspecialedition/mods";
    let (base, _) = mock_nexus(|base| {
        std::collections::HashMap::from([
            ("/v1/users/validate.json".to_string(), json(serde_json::json!({ "user_id": 1, "name": "tester", "is_premium": true }))),
            ("/v2/graphql".to_string(), json(serde_json::json!({ "data": { "collectionRevision": {
                "revisionNumber": 3, "downloadLink": "/v1/collections/abc/revisions/3/download_link.json", "collection": { "name": "Test Pack" },
                "modFiles": [{ "fileId": 11, "optional": false, "file": { "sizeInBytes": "10" } }, { "fileId": 21, "optional": false, "file": { "sizeInBytes": "500" } }, { "fileId": 31, "optional": true, "file": { "sizeInBytes": "7" } }]
            } } }))),
            ("/v1/collections/abc/revisions/3/download_link.json".to_string(), json(serde_json::json!({ "download_links": [{ "URI": format!("{base}/cdn/bundle.zip") }] }))),
            ("/cdn/bundle.zip".to_string(), bundle),
            (format!("{d}/2/files/21.json"), json(serde_json::json!({ "file_id": 21, "file_name": "Enb Stuff-2-1-0-1700000000.zip" }))),
            (format!("{d}/2/files/21/download_link.json"), json(serde_json::json!([{ "URI": format!("{base}/cdn/enb.zip") }]))),
            ("/cdn/enb.zip".to_string(), enb),
        ])
    });
    let mut client = nexus::Client::new("test-key").unwrap();
    client.base = base;

    let mut inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    write(&tmp.path().join("src/Have/Have.esp"), plugin_bytes(&[], false));
    install::install(&inst, &tmp.path().join("src/Have"), None, false, None).unwrap();
    nexus::remember(&inst, "Have", rmm_core::instance::NexusRef { mod_id: 1, file_id: Some(11), ..Default::default() }).unwrap();
    let link = "https://www.nexusmods.com/games/skyrimspecialedition/collections/abc";

    let plan = collection::install(&mut inst, &client, link, None, false, Some("Pack"), true).unwrap();
    assert_eq!((plan.install.clone(), plan.installed.clone(), plan.optional.clone()), (vec!["Enb Stuff".to_string()], vec!["Have".to_string()], vec!["Optional One".to_string()]));
    assert_eq!((plan.game_folder.clone(), plan.download_size, plan.revision), (vec!["Enb Stuff".to_string()], 500, 3));
    assert_eq!(inst.config.profile, "default", "a dry run makes no profile");

    let r = collection::install(&mut inst, &client, link, None, false, Some("Pack"), false).unwrap();
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!((r.profile.as_deref(), r.rules, r.instructions.clone()), (Some("Pack"), 1, vec!["Enb Stuff: Use the high preset.".to_string()]));
    assert_eq!(inst.config.profile, "Pack");
    assert_eq!(Instance::open(&inst.root).unwrap().config.profile, "Pack", "the new profile is the active one");
    let p = inst.profile().unwrap();
    let mods: Vec<(&str, bool)> = p.mods.iter().map(|m| (m.id.as_str(), m.enabled)).collect();
    assert_eq!(mods, [("Have", true), ("Enb Stuff", true)], "the mod already installed is turned on, and the rule orders them");
    let plugin = |p: &rmm_core::Profile, n: &str| p.plugins.iter().find(|e| e.name == n).map(|e| e.enabled);
    assert_eq!((plugin(&p, "Have.esp"), plugin(&p, "Enb.esp")), (Some(true), Some(false)), "the collection's plugin list");
    let files = ["Enb.esp", "Enb.ini", "SKSE/Plugins/x.dll", "root/enblocal.ini", "root/enbseries/a.fx"];
    assert_eq!(inst.mod_files("Enb Stuff").unwrap(), files, "dinput: what isn't Data content goes to the game folder");
    assert!(inst.mod_meta("Enb Stuff").game_folder);
    install::reinstall(&inst, "Enb Stuff", None).unwrap();
    assert_eq!(inst.mod_files("Enb Stuff").unwrap(), files, "a reinstall keeps it there");

    inst.switch_profile("default").unwrap();
    assert_eq!(inst.profile().unwrap().mods.iter().find(|m| m.id == "Have").map(|m| m.enabled), Some(false), "the user's own profile is as it was");
    let again = collection::install(&mut inst, &client, link, None, false, Some("Pack"), true);
    assert!(format!("{:#}", again.unwrap_err()).contains("already exists"));
}

/// The getting-started list's essentials: found in an enabled mod or the game folder, gone with
/// a disabled mod; and a game folder picked by hand (or its Data folder) is recognised.
#[test]
fn essentials_and_a_chosen_game_folder() {
    isolate();
    use rmm_core::{game, launch};
    let tmp = tempfile::tempdir().unwrap();
    let g = tmp.path().join("game");
    write(&g.join("SkyrimSE.exe"), "exe");
    write(&g.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    assert_eq!(game::identify(&g).map(|d| (d.game, d.game_path)), Some((Game::SkyrimSe, g.clone())));
    assert_eq!(game::identify(&g.join("Data")).map(|d| d.game_path), Some(g.clone()), "its Data folder counts");
    assert!(game::identify(tmp.path()).is_none());

    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &g, &tmp.path().join("local")).unwrap();
    let have = |inst: &Instance| launch::essentials(inst).unwrap().items.into_iter().map(|i| (i.what, i.have, i.nexus_id)).collect::<Vec<_>>();
    assert_eq!(have(&inst), [("script_extender", None, Some(30379)), ("address_library", None, Some(32444)), ("crash_logger", None, Some(59818))]);
    assert!(!launch::essentials(&inst).unwrap().played);

    let src = tmp.path().join("src");
    write(&src.join("SKSE/skse64_loader.exe"), "loader");
    write(&src.join("SKSE/Data/Scripts/a.pex"), "pex");
    write(&src.join("AL/SKSE/Plugins/versionlib-1-6-1170-0.bin"), "bin");
    for m in ["SKSE", "AL"] {
        install::install(&inst, &src.join(m), None, true, None).unwrap();
    }
    write(&g.join("Data/SKSE/Plugins/CrashLogger.dll"), "dll"); // put in by hand
    let some = |s: &str| Some(s.to_string());
    assert_eq!(have(&inst).into_iter().map(|x| x.1).collect::<Vec<_>>(), [some("SKSE"), some("AL"), some("the game folder")]);
    ops::set_mods_enabled(&inst, &["AL".into()], false).unwrap();
    assert_eq!(have(&inst)[1].1, None, "a disabled mod doesn't count");
    write(&inst.root.join("game-version.txt"), "1.6.1170.0");
    assert!(launch::essentials(&inst).unwrap().played);
}

#[test]
fn nexus_updates_and_downloads() {
    isolate();
    use rmm_core::nexus;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    let mut zip_bytes = Vec::new();
    {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
        z.start_file("textures/a.dds", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"v2").unwrap();
        z.finish().unwrap();
    }
    let d = "/v1/games/skyrimspecialedition/mods";
    let json = |v: serde_json::Value| v.to_string().into_bytes();
    let (base, hits) = mock_nexus(|base| {
        std::collections::HashMap::from([
            ("/v1/users/validate.json".to_string(), json(serde_json::json!({ "user_id": 1, "key": "test-key", "name": "tester", "is_premium": false, "email": "someone@example.com" }))),
            (format!("{d}/updated.json?period=1m"), json(serde_json::json!([{ "mod_id": 9, "latest_file_update": 1 }]))),
            (format!("{d}/9/files.json"), json(serde_json::json!({
                "files": [{ "file_id": 1, "version": "1.0", "category_name": "OLD_VERSION", "file_name": "A-1.zip" }, { "file_id": 2, "version": "2.0", "category_name": "MAIN", "file_name": "A-2.zip" }],
                "file_updates": [{ "old_file_id": 1, "new_file_id": 2 }]
            }))),
            (format!("{d}/9.json"), json(serde_json::json!({ "name": "Alpha Mod", "category_id": 29 }))),
            ("/v1/games/skyrimspecialedition.json".to_string(), json(serde_json::json!({ "categories": [{ "category_id": 29, "name": "Armour", "parent_category": false }] }))),
            (format!("{d}/9/files/2.json"), json(serde_json::json!({ "file_id": 2, "file_name": "A-2.zip", "version": "2.0" }))),
            (format!("{d}/9/files/2/download_link.json?key=k&expires=5"), json(serde_json::json!([{ "name": "CDN", "URI": format!("{base}/cdn/A-2.zip") }]))),
            ("/cdn/A-2.zip".to_string(), zip_bytes),
        ])
    });
    let client = |key: &str| {
        let mut c = nexus::Client::new(key).unwrap();
        c.base = base.clone();
        c
    };

    let who = client("test-key").validate().unwrap();
    assert_eq!(who["name"], "tester");
    assert!(who.get("key").is_none() && who.get("email").is_none(), "the key and email never leave validate: {who}");
    assert!(format!("{:#}", client("wrong").validate().unwrap_err()).contains("refused the API key"));

    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    write(&tmp.path().join("A/textures/a.dds"), "v1");
    install::install(&inst, &tmp.path().join("A"), None, true, None).unwrap();
    let mut meta = inst.mod_meta("A");
    meta.nexus = Some(rmm_core::instance::NexusRef { mod_id: 9, file_id: Some(1), version: Some("1.0".into()), category: None });
    inst.write_mod_meta("A", &meta).unwrap();

    let c = client("test-key");
    let ups = nexus::check_updates(&inst, &c, None).unwrap();
    assert_eq!(ups.iter().map(|u| (u.id.as_str(), u.file_id, u.latest.as_deref())).collect::<Vec<_>>(), [("A", 2, Some("2.0"))]);
    nexus::check_updates(&inst, &c, None).unwrap();
    assert_eq!(hits.lock().unwrap()[&format!("{d}/9/files.json")], 1, "the second check uses the cache");

    // The mod list shows the version and the update the check found; holds leave it out.
    let row = || ops::list_mods(&inst).unwrap().into_iter().find(|m| m.id == "A").unwrap();
    assert_eq!((row().version.as_deref(), row().update.map(|u| u.file_id)), (Some("1.0"), Some(2)));
    let hold = |h| nexus::hold_updates(&inst, &["A".to_string()], h);
    hold(nexus::Hold::Skip).unwrap();
    assert!(row().update.is_none() && nexus::check_updates(&inst, &c, None).unwrap().is_empty(), "the skipped file isn't offered");
    let mut later = nexus::saved_updates(&inst)["A"].clone();
    later.file_id = 3;
    assert!(nexus::offered(&later, &inst.mod_meta("A")), "a later file is");
    hold(nexus::Hold::Pin).unwrap();
    hold(nexus::Hold::Release).unwrap();
    assert_eq!((inst.mod_meta("A").skip_file, row().update.map(|u| u.file_id)), (None, Some(2)), "release clears the skip too");
    hold(nexus::Hold::Pin).unwrap();
    assert!(row().pinned && row().update.is_none() && nexus::check_updates(&inst, &c, None).unwrap().is_empty(), "a pinned mod isn't offered updates");
    hold(nexus::Hold::Release).unwrap();
    assert_eq!(nexus::check_updates(&inst, &c, None).unwrap().len(), 1);
    assert!(nexus::hold_updates(&inst, &["Nope".to_string()], nexus::Hold::Pin).is_err());
    let mut meta = inst.mod_meta("A");
    meta.nexus.as_mut().unwrap().file_id = Some(2);
    inst.write_mod_meta("A", &meta).unwrap();
    assert!(row().update.is_none(), "installed since the check");

    let link = nexus::parse_nxm("nxm://skyrimspecialedition/mods/9/files/2?key=k&expires=5&user_id=1").unwrap();
    let r = nexus::install(&inst, &c, &link, None).unwrap();
    assert_eq!(r.name, "Alpha Mod");
    assert!(inst.root.join("downloads/A-2.zip").is_file());
    let meta = inst.mod_meta(&r.id);
    let n = meta.nexus.unwrap();
    assert_eq!((n.mod_id, n.file_id, n.version.as_deref(), n.category), (9, Some(2), Some("2.0"), Some(29)));
    assert_eq!(ops::list_mods(&inst).unwrap().iter().find(|m| m.id == r.id).unwrap().categories, ["Armour"]);
    assert_eq!(hits.lock().unwrap()["/v1/games/skyrimspecialedition.json"], 1, "category names are read once");
    assert!(meta.sha256.is_some());
    assert!(nexus::download(&inst, &c, &nexus::parse_nxm("nxm://fallout4/mods/1/files/2").unwrap()).is_err(), "another game");
    let ids = nexus::resolve(&inst, &c, " 9 / 2 ").unwrap();
    assert_eq!((ids.game.as_str(), ids.mod_id, ids.file_id, ids.key), ("skyrimspecialedition", 9, 2, None));
    assert_eq!(nexus::resolve(&inst, &c, "nxm://skyrimspecialedition/mods/9/files/2").unwrap().file_id, 2);
    let ids = |what: &str| nexus::resolve(&inst, &c, what).map(|l| (l.mod_id, l.file_id));
    assert_eq!(ids("9").unwrap(), (9, 2), "a bare mod id takes the newest main file");
    assert_eq!(ids("https://www.nexusmods.com/skyrimspecialedition/mods/9?tab=files").unwrap(), (9, 2));
    assert_eq!(ids("https://www.nexusmods.com/SkyrimSpecialEdition/mods/9?tab=files&file_id=1").unwrap(), (9, 1));
    assert!(format!("{:#}", ids("https://www.nexusmods.com/fallout4/mods/9").unwrap_err()).contains("this instance is skyrimspecialedition"));
    let refused = format!("{:#}", nexus::download(&inst, &c, &nexus::resolve(&inst, &c, "9/2").unwrap()).unwrap_err());
    assert!(refused.contains("free Nexus accounts") && refused.contains("mods/9?tab=files"), "{refused}");
}

#[cfg(target_os = "linux")]
#[test]
fn vfs_mount_layers_and_writes_to_overwrite() {
    isolate();
    use rmm_core::vfs;
    if !Path::new("/dev/fuse").exists() || std::process::Command::new("fusermount3").arg("-V").output().is_err() {
        eprintln!("skipped: no FUSE here");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    let data = game.join("Data");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&data.join("Skyrim.esm"), plugin_bytes(&[], true));
    write(&data.join("textures/v.dds"), "vanilla");
    write(&data.join("textures/shared.dds"), "vanilla-shared");
    fs::create_dir_all(tmp.path().join("local")).unwrap();
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    write(&tmp.path().join("A/textures/shared.dds"), "A");
    write(&tmp.path().join("A/textures/a.dds"), "a");
    write(&tmp.path().join("B/Textures/Shared.dds"), "B");
    write(&tmp.path().join("B/meshes/b.nif"), "b");
    write(&tmp.path().join("B/root/skse_loader.exe"), "loader");
    install::install(&inst, &tmp.path().join("A"), None, true, None).unwrap();
    install::install(&inst, &tmp.path().join("B"), None, true, None).unwrap();
    write(&inst.root.join("overwrite/textures/o.dds"), "o");
    deploy::deploy(&inst, false).unwrap(); // a hard-link deploy first: the mount takes Data back out
    let read = |rel: &str| fs::read_to_string(data.join(rel)).unwrap();

    let m = vfs::mount(&inst).unwrap();
    assert_eq!(read("textures/shared.dds"), "B", "the later mod wins");
    assert_eq!((read("textures/v.dds"), read("textures/a.dds"), read("textures/o.dds"), read("meshes/b.nif")), ("vanilla".into(), "a".into(), "o".into(), "b".into()));
    assert_eq!(read("TEXTURES/A.DDS"), "a", "names match without case");
    let mut names: Vec<String> = fs::read_dir(data.join("textures")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_lowercase()).collect();
    names.sort();
    assert_eq!(names, ["a.dds", "o.dds", "shared.dds", "v.dds"]);
    assert_eq!(read("../skse_loader.exe"), "loader", "root files still deploy");
    assert!(deploy::deploy(&inst, true).unwrap_err().to_string().contains("rmm vfs"), "no deploy while mounted");

    // Writes land in overwrite/; vanilla and mod files stay as they were.
    write(&data.join("new/x.txt"), "new");
    assert_eq!(fs::read_to_string(inst.root.join("overwrite/new/x.txt")).unwrap(), "new");
    fs::OpenOptions::new().append(true).open(data.join("textures/v.dds")).unwrap().write_all(b"+").unwrap();
    fs::write(data.join("textures/a.dds"), "a2").unwrap();
    assert_eq!((read("textures/v.dds"), read("textures/a.dds")), ("vanilla+".into(), "a2".into()));
    assert_eq!(fs::read_to_string(inst.root.join("overwrite/textures/v.dds")).unwrap(), "vanilla+");
    assert_eq!(fs::read_to_string(inst.mod_dir("A").join("textures/a.dds")).unwrap(), "a", "the mod's file is untouched");
    fs::rename(data.join("new/x.txt"), data.join("new/y.txt")).unwrap();
    assert!(inst.root.join("overwrite/new/y.txt").is_file() && !inst.root.join("overwrite/new/x.txt").exists());
    fs::remove_file(data.join("new/y.txt")).unwrap();
    assert!(!inst.root.join("overwrite/new/y.txt").exists());
    fs::remove_file(data.join("meshes/b.nif")).unwrap();
    assert!(!data.join("meshes/b.nif").exists() && inst.mod_dir("B").join("meshes/b.nif").is_file(), "hidden, not deleted");

    drop(m);
    assert_eq!(read("textures/v.dds"), "vanilla", "vanilla Data is back, unchanged");
    assert!(!data.join("textures/a.dds").exists() && vfs::mounted_by(&inst).is_none());
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(read("textures/v.dds"), "vanilla+", "overwrite/ wins a normal deploy too");
    let dirs: Vec<String> = fs::read_dir(&data).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.eq_ignore_ascii_case("textures")).collect();
    assert_eq!(dirs, ["textures"], "B's Textures/ goes into vanilla's textures/, not beside it");
    assert_eq!(read("textures/shared.dds"), "B");
    assert!(inst.root.join("backup/Data/textures/shared.dds").is_file(), "vanilla's file is backed up");
}

/// A tiny Extism add-on in WebAssembly text. `call_host` hands its input to rmm's host function
/// and returns the answer; each (name, json, via_host) is a function returning that JSON, or
/// passing it to the host function and returning the answer.
fn wat_addon(fixed: &[(&str, &str, bool)]) -> String {
    let (mut data, mut funcs, mut at) = (String::new(), String::new(), 0);
    for (name, json, via_host) in fixed {
        let esc: String = json.bytes().map(|b| format!("\\{b:02x}")).collect();
        data += &format!("(data (i32.const {at}) \"{esc}\")\n");
        let len = json.len();
        funcs += &if *via_host {
            format!("(func (export \"{name}\") (result i32) (local $r i64) (local.set $r (call $rmm (call $copy (i32.const {at}) (i32.const {len})))) (call $output_set (local.get $r) (call $length (local.get $r))) (i32.const 0))\n")
        } else {
            format!("(func (export \"{name}\") (result i32) (call $output_set (call $copy (i32.const {at}) (i32.const {len})) (i64.const {len})) (i32.const 0))\n")
        };
        at += len;
    }
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (import "extism:host/env" "length" (func $length (param i64) (result i64)))
  (import "extism:host/env" "input_length" (func $input_length (result i64)))
  (import "extism:host/env" "input_load_u8" (func $input_load_u8 (param i64) (result i32)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/user" "rmm" (func $rmm (param i64) (result i64)))
  (memory 1)
  {data}
  (func $copy (param $ptr i32) (param $len i32) (result i64)
    (local $off i64) (local $i i32)
    (local.set $off (call $alloc (i64.extend_i32_u (local.get $len))))
    (block $done (loop $next
      (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
      (call $store_u8 (i64.add (local.get $off) (i64.extend_i32_u (local.get $i))) (i32.load8_u (i32.add (local.get $ptr) (local.get $i))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $next)))
    (local.get $off))
  (func $input (result i64)
    (local $n i64) (local $off i64) (local $i i64)
    (local.set $n (call $input_length))
    (local.set $off (call $alloc (local.get $n)))
    (block $done (loop $next
      (br_if $done (i64.ge_u (local.get $i) (local.get $n)))
      (call $store_u8 (i64.add (local.get $off) (local.get $i)) (call $input_load_u8 (local.get $i)))
      (local.set $i (i64.add (local.get $i) (i64.const 1)))
      (br $next)))
    (local.get $off))
  (func (export "call_host") (result i32)
    (local $r i64)
    (local.set $r (call $rmm (call $input)))
    (call $output_set (local.get $r) (call $length (local.get $r)))
    (i32.const 0))
  {funcs})"#
    )
}

#[test]
fn addons_hooks_and_capabilities() {
    isolate();
    use rmm_core::addons;
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    fs::create_dir_all(tmp.path().join("local")).unwrap();
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();

    let helper = tmp.path().join("helper");
    write(&helper.join("addon.toml"), r#"
name = "helper"
version = "1.0"
capabilities = ["read_instance", "mod_files"]
hooks = ["after_install", "health_check"]
tools = [{ name = "call_host", description = "Pass a request to rmm" }]
panels = [{ title = "Status", function = "status" }]
"#);
    write(&helper.join("addon.wasm"), wat_addon(&[
        ("after_install", r#"{"op":"write_overwrite","path":"hook/installed.txt","text":"yes"}"#, true),
        ("health_check", r#"[{"severity":"error","subject":"Test","message":"from wasm"}]"#, false),
        ("status", r#"{"text":"all good"}"#, false),
    ]));
    let preview = addons::install(&inst, &helper, true).unwrap();
    assert_eq!(serde_json::to_value(&preview.toml.capabilities).unwrap(), json!(["read_instance", "mod_files"]));
    assert!(addons::list(&inst).unwrap().is_empty(), "a dry run installs nothing");
    addons::install(&inst, &helper, false).unwrap();

    write(&tmp.path().join("A/textures/a.dds"), "a");
    let r = install::install(&inst, &tmp.path().join("A"), None, true, None).unwrap();
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert_eq!(fs::read_to_string(inst.root.join("overwrite/hook/installed.txt")).unwrap(), "yes", "after_install ran");

    let host = |req: serde_json::Value| addons::call(&inst, "helper", "call_host", &req).unwrap();
    assert_eq!(host(json!({ "op": "list_mods" }))["ok"][0]["id"], "A");
    let denied = host(json!({ "op": "set_mods_enabled", "ids": ["A"], "enabled": false }));
    assert!(denied["error"].as_str().unwrap().contains("modify_profile"), "{denied}");
    assert!(ops::list_mods(&inst).unwrap()[0].enabled, "nothing changed");
    for escape in [json!({ "op": "read_file", "id": "..", "path": "instance.toml" }), json!({ "op": "write_overwrite", "path": "../escape.txt", "text": "x" })] {
        assert!(host(escape.clone())["error"].as_str().unwrap().contains(".."), "{escape}");
    }
    assert!(!inst.root.join("escape.txt").exists());
    assert_eq!(host(json!({ "op": "read_file", "id": "A", "path": "textures/a.dds" }))["ok"], "YQ==");
    assert_eq!(addons::call(&inst, "helper", "status", &json!({})).unwrap()["text"], "all good");
    assert!(addons::call(&inst, "helper", "after_install", &json!({})).is_err(), "hooks aren't callable as tools");

    let issues = health::check(&inst).unwrap();
    assert!(issues.iter().any(|i| i.kind == "addon" && i.severity == "error" && i.message.contains("from wasm")), "{issues:?}");

    let gate = tmp.path().join("gate");
    write(&gate.join("addon.toml"), "name = \"gate\"\nversion = \"1\"\nhooks = [\"before_deploy\"]\n");
    write(&gate.join("addon.wasm"), wat_addon(&[("before_deploy", r#"{"abort":"not today"}"#, false)]));
    addons::install(&inst, &gate, false).unwrap();
    assert!(deploy::deploy(&inst, true).is_ok(), "previews don't run hooks");
    assert!(format!("{:#}", deploy::deploy(&inst, false).unwrap_err()).contains("not today"));
    assert!(!game.join("Data/textures/a.dds").exists());
    addons::remove(&inst, "gate").unwrap();
    deploy::deploy(&inst, false).unwrap();
    assert!(game.join("Data/textures/a.dds").exists());

    write(&tmp.path().join("bad/addon.toml"), "name = \"bad\"\nversion = \"1\"\nhosts = [\"example.com\"]\n");
    write(&tmp.path().join("bad/addon.wasm"), "(module)");
    assert!(addons::install(&inst, &tmp.path().join("bad"), true).unwrap_err().to_string().contains("network"));
}

#[test]
fn a_folder_keeps_the_dots_in_its_name() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("game/SkyrimSE.exe"), "exe");
    write(&tmp.path().join("Lib (1.7.104.0) v13/SKSE/Plugins/x.bin"), "x");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &tmp.path().join("game"), &tmp.path().join("local")).unwrap();
    let r = install::install(&inst, &tmp.path().join("Lib (1.7.104.0) v13"), None, true, None).unwrap();
    assert_eq!(r.name, "Lib (1.7.104.0) v13");

    // A Nexus download's name gives the mod's name, and its Nexus id and version for update checks.
    for (download, name, id, version) in [
        ("Address Library All in One (1.7.104.0) v13 32444 13 2026-08-27T15-29Z Ae46W7Fw2", "Address Library All in One (1.7.104.0) v13", 32444, "13"),
        ("Unofficial Skyrim Special Edition Patch-266-4-2-5a-1623868815", "Unofficial Skyrim Special Edition Patch", 266, "4.2.5a"),
    ] {
        write(&tmp.path().join(download).join("SKSE/Plugins/x.bin"), "x");
        let r = install::install(&inst, &tmp.path().join(download), None, true, None).unwrap();
        let nx = inst.mod_meta(&r.id).nexus.unwrap();
        assert_eq!((r.name.as_str(), nx.mod_id, nx.version.as_deref()), (name, id, Some(version)));
    }
}

/// A game from an add-on: Skyrim VR on Skyrim SE's rules, with its own exe, folders and LOOT type.
#[test]
fn a_game_from_an_addon() {
    isolate();
    use rmm_core::{addons, archives};
    let tmp = tempfile::tempdir().unwrap();
    let vr = tmp.path().join("vr-addon");
    write(&vr.join("addon.toml"), r#"
name = "skyrim-vr"
version = "1.0"

[[games]]
id = "skyrimvr"
name = "Skyrim VR"
base = "skyrim-se"
loadorder = "SkyrimVR"
exe = "SkyrimVR.exe"
loader = "sksevr_loader.exe"
steam_app_id = 611670
steam_dir = "SkyrimVR"
local_dir = "Skyrim VR"
ini = "SkyrimVR.ini"
nexus_domain = "skyrimspecialedition"
"#);
    let games = addons::games_in(&vr).unwrap();
    let g = games[0];
    assert_eq!((g.id(), g.base(), g.exe(), g.loader(), g.steam_app_id()), ("skyrimvr", Game::SkyrimSe, "SkyrimVR.exe", "sksevr_loader.exe", 611670));
    assert_eq!((g.lo_id(), g.local_folder(tmp.path())), (loadorder::GameId::SkyrimVR, "Skyrim VR".to_string()));
    assert!(archives::loaded_by(g, "Foo.esp", "Foo - Textures.bsa"), "Skyrim SE's archive rules");
    assert_eq!("SkyrimVR".parse::<Game>().unwrap(), g);

    let game = tmp.path().join("game");
    write(&game.join("SkyrimVR.exe"), "exe");
    let inst = Instance::create(&tmp.path().join("inst"), g, &game, &tmp.path().join("local")).unwrap();
    addons::install(&inst, &vr, false).unwrap();
    assert!(fs::read_to_string(tmp.path().join("inst/instance.toml")).unwrap().contains(r#"game = "skyrimvr""#));
    assert_eq!(Instance::open(&tmp.path().join("inst")).unwrap().config.game, g);
    assert_eq!(addons::list(&inst).unwrap()[0].toml.games[0].name, "Skyrim VR");

    let def = |id: &str, base: &str, lo: &str| rmm_core::game::GameDef {
        id: id.into(), name: "x".into(), base: base.into(), loadorder: Some(lo.into()), exe: "x.exe".into(), loader: "l.exe".into(),
        steam_app_id: 1, steam_dir: "x".into(), local_dir: "x".into(), ini: "x.ini".into(), nexus_domain: "x".into(),
    };
    assert!(rmm_core::game::register(def("oblivion-x", "oblivion", "SkyrimSE")).is_err());
    assert!(rmm_core::game::register(def("vr-x", "skyrim-se", "Fallout4VR")).is_err());
    assert!(rmm_core::game::register(def("skyrim-se", "skyrim-se", "SkyrimSE")).is_err());
}

/// A plugin with WEAP records of these form ids (top byte: master index, as the game reads it).
fn with_records(mut plugin: Vec<u8>, ids: &[u32]) -> Vec<u8> {
    let mut recs = Vec::new();
    for id in ids {
        recs.extend(b"WEAP");
        recs.extend(0u32.to_le_bytes()); // data size
        recs.extend(0u32.to_le_bytes()); // flags
        recs.extend(id.to_le_bytes());
        recs.extend([0u8; 4]);
        recs.extend(44u16.to_le_bytes());
        recs.extend(0u16.to_le_bytes());
    }
    plugin.extend(b"GRUP");
    plugin.extend((24 + recs.len() as u32).to_le_bytes());
    plugin.extend(b"WEAP");
    plugin.extend([0u8; 12]);
    plugin.extend(recs);
    plugin
}

#[test]
fn plugin_record_conflicts() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), with_records(plugin_bytes(&[], true), &[0xD62, 0xD63]));
    // A and B both change Skyrim.esm's 0xD62; C is a patch changing A's own new record.
    write(&tmp.path().join("A/A.esp"), with_records(plugin_bytes(&["Skyrim.esm"], false), &[0xD62, 0x0100_0800]));
    write(&tmp.path().join("B/B.esp"), with_records(plugin_bytes(&["Skyrim.esm"], false), &[0xD62, 0xD63, 0x0100_0801]));
    write(&tmp.path().join("C/C.esp"), with_records(plugin_bytes(&["Skyrim.esm", "A.esp"], false), &[0x0100_0800]));
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    for m in ["A", "B", "C"] {
        install::install(&inst, &tmp.path().join(m), None, true, None).unwrap();
    }
    let p = inst.profile().unwrap();
    let pairs = |only| plugins::record_overlaps(&inst, &p, only).unwrap().iter().map(|o| format!("{}<{} {} {}", o.plugin, o.wins, o.records, o.patch)).collect::<Vec<_>>();
    assert_eq!(pairs(None), ["A.esp<B.esp 1 false", "A.esp<C.esp 1 true"], "Skyrim.esm is left out; B's 0xD63 clashes with nothing else");
    assert_eq!(pairs(Some("c.esp")), ["A.esp<C.esp 1 true"]);
    assert!(plugins::record_overlaps(&inst, &p, Some("Skyrim.esm")).is_err());
}

#[cfg(windows)]
#[test]
fn scans_and_reads_mod_files() {
    isolate();
    use rmm_core::inspect;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    // The test program stands in for a DLL: a real PE file with imports.
    let pe = fs::read(std::env::current_exe().unwrap()).unwrap();
    write(&tmp.path().join("M/SKSE/Plugins/m.dll"), &pe);
    write(&tmp.path().join("M/textures/fake.dds"), &pe);
    write(&tmp.path().join("M/run.bat"), "@echo off\r\nstart https://example.com\r\n");
    write(&tmp.path().join("M/M.ini"), "[General]\r\nfSpeed=1.5\r\n");
    write(&tmp.path().join("Plain/textures/a.dds"), "DDS ");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &tmp.path().join("M"), None, true, None).unwrap();
    install::install(&inst, &tmp.path().join("Plain"), None, true, None).unwrap();

    let scans = inspect::scan(&inst, None).unwrap();
    assert_eq!(scans.iter().map(|s| s.mod_id.as_str()).collect::<Vec<_>>(), ["M"], "plain texture mods aren't listed");
    let s = &scans[0];
    assert_eq!((s.verdict, s.defender.is_none()), ("threat", true));
    assert_eq!(s.native_code, ["SKSE/Plugins/m.dll", "textures/fake.dds"]);
    let has = |file: &str, sev: &str, text: &str| s.findings.iter().any(|f| f.file == file && f.severity == sev && f.message.contains(text));
    assert!(has("textures/fake.dds", "danger", "posing as a .dds"), "{:#?}", s.findings);
    assert!(has("run.bat", "warning", "runs outside the game"), "{:#?}", s.findings);
    assert!(has("SKSE/Plugins/m.dll", "info", "exports no SKSEPlugin_Load"), "{:#?}", s.findings);

    let ini = inspect::read_file(&inst, "M", "M.ini", 1, 5).unwrap();
    assert_eq!((ini.kind, ini.text.as_deref(), ini.total_lines), ("text", Some("fSpeed=1.5"), Some(2)));
    let dll = inspect::read_file(&inst, "M", "SKSE/Plugins/m.dll", 0, 5).unwrap();
    assert_eq!(dll.kind, "native");
    assert!(dll.native.unwrap().imports.contains_key("kernel32.dll"));
    assert!(inspect::read_file(&inst, "M", "../Plain/textures/a.dds", 0, 5).is_err());
    assert!(inspect::read_file(&inst, "M", "nope.txt", 0, 5).is_err());
    assert_eq!(inspect::read_file(&inst, "Plain", "textures/a.dds", 0, 5).unwrap().kind, "text", "4 printable bytes read as text");
}

/// A record with subrecords; flags 0x40000 packs its data with zlib, as the game does.
fn record(kind: &[u8; 4], flags: u32, id: u32, subs: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let mut data = Vec::new();
    for (t, b) in subs {
        data.extend(*t);
        data.extend((b.len() as u16).to_le_bytes());
        data.extend(*b);
    }
    if flags & 0x40000 != 0 {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&data).unwrap();
        let mut packed = (data.len() as u32).to_le_bytes().to_vec();
        packed.extend(z.finish().unwrap());
        data = packed;
    }
    let mut r = kind.to_vec();
    r.extend((data.len() as u32).to_le_bytes());
    r.extend(flags.to_le_bytes());
    r.extend(id.to_le_bytes());
    r.extend([0u8; 4]);
    r.extend(44u16.to_le_bytes());
    r.extend(0u16.to_le_bytes());
    r.extend(data);
    r
}

fn group(label: &[u8; 4], records: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = records.concat();
    let mut g = b"GRUP".to_vec();
    g.extend((24 + body.len() as u32).to_le_bytes());
    g.extend(label);
    g.extend([0u8; 12]);
    g.extend(body);
    g
}

#[test]
fn plugin_new_records() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let mut esp = plugin_bytes(&["Skyrim.esm"], false);
    esp.extend(group(b"QUST", &[record(b"QUST", 0, 0x0100_0800, &[(b"EDID", b"MyQuest\0"), (b"FULL", b"A Quest\0")])]));
    esp.extend(group(b"NPC_", &[record(b"NPC_", 0x40000, 0x0100_0801, &[(b"EDID", b"MyBandit\0")])]));
    esp.extend(group(b"WEAP", &[record(b"WEAP", 0, 0xD62, &[(b"EDID", b"IronSword\0")])]));
    esp.extend(group(b"CELL", &[group(&[0; 4], &[record(b"CELL", 0, 0x0100_0802, &[])])]));
    write(&tmp.path().join("M/M.esp"), esp);
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &tmp.path().join("M"), None, true, None).unwrap();
    let records = |kinds: &[String]| plugins::new_records(&inst, &inst.profile().unwrap(), "m.esp", kinds, 300).unwrap();
    let r = records(&[]);
    let got: Vec<_> = r.records.iter().map(|x| format!("{} {} {:?} {:?}", x.kind, x.editor_id, x.name, x.form_id)).collect();
    assert_eq!(got, [r#"QUST MyQuest Some("A Quest") Some("01000800")"#, r#"NPC_ MyBandit None Some("01000801")"#], "the packed NPC_ unpacks; Skyrim.esm is 00");
    assert_eq!(r.counts.get("CELL"), Some(&1), "an unnamed cell is counted, not listed");
    assert!(!r.counts.contains_key("WEAP"), "changes to the master's records are left out");
    assert_eq!(records(&["npc".into()]).records.len(), 1, "NPC means NPC_");
    ops::set_plugin_enabled(&inst, "M.esp", false).unwrap();
    assert_eq!(records(&[]).records[0].form_id, None, "no load order slot while disabled");
    assert!(plugins::new_records(&inst, &inst.profile().unwrap(), "Nope.esp", &[], 300).is_err());
}

#[test]
fn reviews_follow_the_files() {
    isolate();
    use rmm_core::inspect;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    write(&tmp.path().join("A/textures/a.dds"), "x");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &tmp.path().join("A"), None, true, None).unwrap();
    assert!(inspect::save_review(&inst, "A", "fine", "Only textures.", "").is_err(), "verdicts are clean, suspicious or malicious");
    assert!(inspect::save_review(&inst, "A", "clean", " ", "").is_err(), "a review needs notes");
    assert!(inspect::save_review(&inst, "Nope", "clean", "x", "").is_err());
    let r = inspect::save_review(&inst, "A", "Clean", "Only textures.", "Anywhere.").unwrap();
    assert_eq!((r.review.verdict.as_str(), r.current), ("clean", true));
    assert!(inspect::scan(&inst, None).unwrap().iter().any(|s| s.review.as_ref().is_some_and(|r| r.current)), "the triage shows reviewed mods");
    // Reinstalled from a source that changed: the review stays, out of date.
    write(&tmp.path().join("A/textures/b.dds"), "new");
    install::reinstall(&inst, "A", None).unwrap();
    let all = inspect::reviews(&inst, None).unwrap();
    assert_eq!((all.len(), all[0].current, all[0].review.notes.as_str()), (1, false, "Only textures."));
}

#[test]
fn nexus_search_and_mod_pages() {
    isolate();
    use rmm_core::nexus;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    write(&tmp.path().join("Campfire/textures/a.dds"), "x");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &tmp.path().join("Campfire"), None, true, None).unwrap();
    nexus::remember(&inst, "Campfire", rmm_core::instance::NexusRef { mod_id: 667, ..Default::default() }).unwrap();
    let node = |id: u64, name: &str| serde_json::json!({ "modId": id, "name": name, "summary": "s", "category": "Immersion", "author": "a", "version": "1", "endorsements": 5, "downloads": 9, "updatedAt": "2026-01-01T00:00:00Z", "pictureUrl": null });
    let mut frostfall = node(671, "Frostfall");
    frostfall["gameId"] = 1704.into();
    frostfall["description"] = "[b]Needs[/b] [url=https://www.nexusmods.com/skyrimspecialedition/mods/667]Campfire[/url].".into();
    frostfall["tags"] = serde_json::json!([{ "name": "Gameplay" }]);
    frostfall["modRequirements"] = serde_json::json!({
        "dlcRequirements": [],
        "nexusRequirements": { "nodes": [{ "modId": "667", "modName": "Campfire", "notes": "", "url": "", "externalRequirement": false, "gameId": "1704" }] },
        "modsRequiringThisMod": { "totalCount": 2, "nodes": [
            { "modId": "3502", "modName": "A patch", "notes": "", "url": "", "externalRequirement": false, "gameId": "1704" },
            { "modId": "12", "modName": "For another game", "notes": "", "url": "", "externalRequirement": false, "gameId": "110" }
        ] }
    });
    // Every query reads its own part of the one answer.
    let body = serde_json::json!({ "data": { "game": { "id": 1704 }, "mods": { "totalCount": 2, "nodes": [node(671, "Frostfall"), node(667, "Campfire")] }, "mod": frostfall } });
    let (base, hits) = mock_nexus(|_| std::collections::HashMap::from([("/v2/graphql".to_string(), body.to_string().into_bytes())]));
    let mut client = nexus::Client::anonymous();
    client.base = base;
    let found = nexus::search(&inst, &client, &nexus::Search { query: "camping".into(), ..Default::default() }).unwrap();
    assert_eq!((found.total, found.mods[0].url.as_str()), (2, "https://www.nexusmods.com/skyrimspecialedition/mods/671"));
    assert_eq!((found.mods[0].installed.as_deref(), found.mods[1].installed.as_deref()), (None, Some("Campfire")));
    let info = nexus::mod_info(&inst, &client, 671).unwrap();
    assert_eq!(info.description, "Needs https://www.nexusmods.com/skyrimspecialedition/mods/667 Campfire.");
    assert_eq!((info.requires[0].needed.mod_id, info.requires[0].installed.as_deref()), (Some(667), Some("Campfire")));
    assert_eq!((info.required_by_count, info.required_by.len(), info.tags.as_slice()), (2, 1, &["Gameplay".to_string()][..]), "other games' mods are left out");
    assert_eq!(hits.lock().unwrap()["/v2/graphql"], 3);

    // Frostfall's requirements are kept; Problems says so once Campfire is disabled.
    write(&tmp.path().join("Frostfall/Frostfall.esp"), plugin_bytes(&["Skyrim.esm"], false));
    install::install(&inst, &tmp.path().join("Frostfall"), None, true, None).unwrap();
    nexus::remember(&inst, "Frostfall", rmm_core::instance::NexusRef { mod_id: 671, ..Default::default() }).unwrap();
    nexus::refresh_requirements(&inst, &client, &[671]).unwrap();
    assert_eq!(nexus::requirements(&inst)[&671][0].name, "Campfire");
    let needs = |inst: &Instance| health::check(inst).unwrap().into_iter().filter(|i| i.kind == "nexus_requirement").map(|i| (i.subject, i.message)).collect::<Vec<_>>();
    assert_eq!(needs(&inst), []);
    ops::set_mods_enabled(&inst, &["Campfire".into()], false).unwrap();
    assert_eq!(needs(&inst), [("Frostfall".into(), "its Nexus page lists as required: Campfire, installed but disabled. Ignore this if you have it another way".into())]);
    fs::write(inst.root.join("nexus-categories.json"), r#"{"29":"Armour"}"#).unwrap();
    let wrong = nexus::search(&inst, &client, &nexus::Search { category: Some("Armor".into()), ..Default::default() }).unwrap_err();
    assert!(format!("{wrong:#}").contains("it has: Armour"), "{wrong:#}");
    assert!(nexus::search(&inst, &client, &nexus::Search { category: Some("armour".into()), sort: Some("newest".into()), ..Default::default() }).is_err(), "unknown sort");
}

/// Which of a mod's Nexus requirements count as missing: by Nexus id (recorded or in the archive's
/// name) or name, the script extender by its loader, and not other sites' downloads.
#[test]
fn reports_missing_nexus_requirements() {
    isolate();
    use rmm_core::nexus;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let src = tmp.path().join("src");
    write(&src.join("SkyUI/interface/skyui.swf"), "x");
    write(&src.join("Address Library for SKSE Plugins-32444-11-1710536800/SKSE/Plugins/versionlib-1-6-1170-0.bin"), "x");
    for m in ["SkyUI", "Address Library for SKSE Plugins-32444-11-1710536800"] {
        install::install(&inst, &src.join(m), None, true, None).unwrap();
    }
    nexus::remember(&inst, "SkyUI", rmm_core::instance::NexusRef { mod_id: 12604, ..Default::default() }).unwrap();
    // Pandora stands in for Nemesis, and a note can say a requirement isn't one.
    write(&src.join("Pandora/Pandora Behaviour Engine+.exe"), "x");
    install::install(&inst, &src.join("Pandora"), None, true, None).unwrap();
    nexus::remember(&inst, "Pandora", rmm_core::instance::NexusRef { mod_id: 133232, ..Default::default() }).unwrap();
    fs::write(
        inst.root.join("nexus-requirements.json"),
        r#"{"12604": [{"name": "SKSE64", "url": "https://skse.silverlock.org/"}, {"mod_id": 32444, "name": "Address Library for SKSE Plugins"},
            {"mod_id": 555, "name": "PapyrusUtil SE"}, {"name": "A tool", "url": "https://example.com/tool"}, {"mod_id": 12604, "name": "SkyUI"},
            {"mod_id": 60033, "name": "Nemesis Unlimited Behavior Engine"}, {"mod_id": 69980, "name": "QuickLoot EE", "notes": "NOT REQUIRED, simply here for visibility"}]}"#,
    )
    .unwrap();
    let needs = |inst: &Instance| health::check(inst).unwrap().into_iter().filter(|i| i.kind == "nexus_requirement").map(|i| (i.subject, i.message)).collect::<Vec<_>>();
    let id = "Address Library for SKSE Plugins";
    assert_eq!(
        needs(&inst),
        [("SkyUI".into(), "its Nexus page lists as required: SKSE64 (skse.silverlock.org), not installed; PapyrusUtil SE (nexusmods.com/skyrimspecialedition/mods/555), not installed. Ignore this if you have them another way".into())]
    );
    ops::set_mods_enabled(&inst, &[id.into()], false).unwrap();
    assert!(needs(&inst)[0].1.contains("(skse.silverlock.org), not installed; Address Library for SKSE Plugins, installed but disabled; PapyrusUtil SE"), "{:?}", needs(&inst));
    ops::set_mods_enabled(&inst, &[id.into()], true).unwrap();
    // The loader in the game folder is the script extender; a mod with the name is the requirement.
    write(&game.join("skse64_loader.exe"), "loader");
    write(&src.join("PapyrusUtil SE/SKSE/Plugins/PapyrusUtil.dll"), "x");
    install::install(&inst, &src.join("PapyrusUtil SE"), None, true, None).unwrap();
    assert_eq!(needs(&inst), []);
    ops::set_mods_enabled(&inst, &["SkyUI".into()], false).unwrap();
    fs::remove_file(game.join("skse64_loader.exe")).unwrap();
    assert_eq!(needs(&inst), [], "a disabled mod's requirements don't matter");
}

/// A DLC requirement counts by its master in Data; Install all installs what's on Nexus and turns
/// on what's off, and leaves the rest to the user.
#[test]
fn installs_lacking_requirements() {
    isolate();
    use rmm_core::nexus;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let src = tmp.path().join("src");
    for m in ["Frostfall", "Campfire"] {
        write(&src.join(m).join(format!("{m}.txt")), m);
        install::install(&inst, &src.join(m), None, true, None).unwrap();
    }
    nexus::remember(&inst, "Frostfall", rmm_core::instance::NexusRef { mod_id: 671, ..Default::default() }).unwrap();
    nexus::remember(&inst, "Campfire", rmm_core::instance::NexusRef { mod_id: 667, ..Default::default() }).unwrap();
    ops::set_mods_enabled(&inst, &["Campfire".into()], false).unwrap();
    fs::write(
        inst.root.join("nexus-requirements.json"),
        r#"{"671": [{"mod_id": 667, "name": "Campfire"}, {"mod_id": 555, "name": "PapyrusUtil SE"}, {"name": "Dawnguard", "dlc": true},
            {"name": "Creation Club: Fishing", "dlc": true}, {"name": "SKSE64", "url": "https://skse.silverlock.org/"}]}"#,
    )
    .unwrap();
    let lacking = || nexus::lacking_requirements(&inst, &inst.profile().unwrap());
    let l = lacking();
    assert_eq!(
        l.iter().map(|l| (l.name.as_str(), l.mod_id, l.disabled.as_deref(), l.dlc)).collect::<Vec<_>>(),
        [("Campfire", Some(667), Some("Campfire"), false), ("PapyrusUtil SE", Some(555), None, false), ("Dawnguard", None, None, true), ("SKSE64", None, None, false)],
        "a DLC rmm can't check is left out"
    );
    let words = health::check(&inst).unwrap().into_iter().find(|i| i.kind == "nexus_requirement").unwrap().message;
    assert!(words.contains("Campfire, installed but disabled; PapyrusUtil SE (nexusmods.com/skyrimspecialedition/mods/555), not installed; the Dawnguard DLC, not in the game's Data folder; SKSE64"), "{words}");
    write(&game.join("Data/Dawnguard.esm"), plugin_bytes(&[], true));
    assert!(!lacking().iter().any(|l| l.dlc));

    let json = |v: serde_json::Value| v.to_string().into_bytes();
    let d = "/v1/games/skyrimspecialedition/mods";
    let mut zip = Vec::new();
    {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut zip));
        z.start_file("SKSE/Plugins/PapyrusUtil.dll", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"dll").unwrap();
        z.finish().unwrap();
    }
    let size = zip.len();
    let (base, _) = mock_nexus(|base| {
        std::collections::HashMap::from([
            ("/v1/users/validate.json".to_string(), json(serde_json::json!({ "user_id": 1, "name": "tester", "is_premium": true }))),
            (format!("{d}/555/files.json"), json(serde_json::json!({ "files": [{ "file_id": 9, "category_name": "MAIN", "uploaded_timestamp": 1 }] }))),
            (format!("{d}/555/files/9.json"), json(serde_json::json!({ "file_id": 9, "file_name": "PapyrusUtil SE-555-4-6-1700000000.zip", "size_in_bytes": size, "version": "4.6" }))),
            (format!("{d}/555/files/9/download_link.json"), json(serde_json::json!([{ "URI": format!("{base}/cdn/pu.zip") }]))),
            ("/cdn/pu.zip".to_string(), zip),
            (format!("{d}/555.json"), json(serde_json::json!({ "name": "PapyrusUtil SE" }))),
        ])
    });
    let mut client = nexus::Client::new("test-key").unwrap();
    client.base = base;
    let r = nexus::install_lacking(&inst, &client).unwrap();
    assert_eq!((r.installed.clone(), r.turned_on.clone(), r.failed.len()), (vec!["PapyrusUtil SE".to_string()], vec!["Campfire".to_string()], 0), "{r:?}");
    assert_eq!(r.left.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["SKSE64"]);
    assert_eq!(inst.mod_meta("PapyrusUtil SE").nexus.map(|n| n.mod_id), Some(555));
    assert_eq!(lacking().iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["SKSE64"]);
}

/// `rmm nxm` leaves links for the window that holds the instance's window lock, in order.
#[test]
fn nxm_links_go_to_the_open_window() {
    use rmm_core::nexus::{self, Handed};
    let tmp = tempfile::tempdir().unwrap();
    let (root, no_app) = (tmp.path(), tmp.path().join("no-window-program.exe"));
    let (a, b) = ("nxm://skyrimspecialedition/mods/1/files/2?key=k&expires=9", "nxm://skyrimspecialedition/mods/3/files/4");
    assert!(!nexus::window_open(root));
    assert_eq!(nexus::hand_to_window(root, a, &no_app).unwrap(), Handed::NoWindow, "no window and no program to start: install here");
    let lock = nexus::hold_window(root).unwrap();
    assert!(nexus::window_open(root));
    assert!(nexus::hold_window(root).is_err(), "one window per instance");
    for l in [a, b] {
        assert_eq!(nexus::hand_to_window(root, l, &no_app).unwrap(), Handed::ToWindow);
    }
    assert!(nexus::hand_to_window(root, "https://example.com/x", &no_app).is_err(), "only nxm links");
    assert_eq!(nexus::pending_links(root), 2);
    assert_eq!(nexus::take_links(root), [a, b]);
    assert_eq!((nexus::pending_links(root), nexus::take_links(root).len()), (0, 0));
    drop(lock);
    assert!(!nexus::window_open(root), "closing the window unlocks it");
}

#[test]
fn workspace_files_edits_and_reverts() {
    isolate();
    use rmm_core::workspace;
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    fs::create_dir_all(game.join("Data")).unwrap();
    write(&tmp.path().join("M/M.ini"), b"[General]\nsName=Caf\xe9\n");
    write(&tmp.path().join("M/scripts/x.pex"), [0xFAu8, 0x57, 0xC0, 0xDE, 0, 0]);
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &tmp.path().join("M"), None, true, None).unwrap();
    let roots: Vec<String> = workspace::list(&inst, "").unwrap().into_iter().map(|e| e.path).collect();
    assert!(roots.starts_with(&["mods".to_string(), "overwrite".to_string()]), "{roots:?}");
    assert_eq!(workspace::list(&inst, "mods").unwrap()[0].path, "mods/M");
    assert!(workspace::list(&inst, "overwrite").unwrap().is_empty(), "no overwrite/ yet");

    let ini = workspace::read(&inst, "mods/M/M.ini").unwrap();
    assert!(ini.editable && ini.text.contains("Café"));
    let e = workspace::write(&inst, "mods/M/M.ini", &ini.text.replace("Café", "Cafés"), Some(&ini.hash), "you").unwrap();
    assert_eq!((e.n, e.lines_before, e.lines_after), (1, 2, 2));
    assert_eq!(fs::read(inst.mod_dir("M").join("M.ini")).unwrap(), b"[General]\nsName=Caf\xe9s\n", "8-bit text stays 8-bit");
    assert!(workspace::write(&inst, "mods/M/M.ini", "x", Some(&ini.hash), "you").is_err(), "changed since it was read");
    assert!(workspace::replace(&inst, "mods/M/M.ini", "nope", "x", "AI").is_err());
    assert!(workspace::replace(&inst, "mods/M/M.ini", "s", "x", "AI").is_err(), "found more than once");
    workspace::replace(&inst, "mods/M/M.ini", "sName=", "sName = ", "AI").unwrap();
    workspace::write(&inst, "overwrite/SKSE/Plugins/new.ini", "[A]\n", None, "AI").unwrap();
    assert!(workspace::write(&inst, "mods/M/scripts/x.pex", "x", None, "AI").is_err(), "compiled files aren't text");
    for bad in ["mods/../x", "mods/M/../../x", "ini/../x.ini", "ini/Skyrim.txt", "C:/x", "elsewhere/x", "mods/Nope/a.ini"] {
        assert!(workspace::read(&inst, bad).is_err(), "{bad}");
    }

    assert_eq!(workspace::edits(&inst).unwrap().iter().map(|e| e.n).collect::<Vec<_>>(), [3, 2, 1], "newest first");
    workspace::revert(&inst, 1).unwrap();
    assert_eq!(fs::read(inst.mod_dir("M").join("M.ini")).unwrap(), b"[General]\nsName=Caf\xe9\n");
    workspace::revert(&inst, 3).unwrap();
    assert!(!inst.mod_dir(rmm_core::instance::OVERWRITE).join("SKSE/Plugins/new.ini").exists(), "a made file goes away again");
    let log = workspace::edits(&inst).unwrap();
    assert_eq!((log.len(), log[0].by.as_str(), log[0].lines_after), (5, "revert 3", 0));
}

#[test]
fn bisect_finds_the_mod_behind_a_crash() {
    use rmm_core::{bisect, rules};
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    write(&tmp.path().join("SKSE/skse64_loader.exe"), "loader");
    write(&tmp.path().join("Base/Base.esm"), plugin_bytes(&["Skyrim.esm"], true));
    write(&tmp.path().join("Patch/Patch.esp"), plugin_bytes(&["Skyrim.esm", "Base.esm"], false));
    for m in ["C", "D", "E", "G"] {
        write(&tmp.path().join(m).join(format!("{m}.esp")), plugin_bytes(&["Skyrim.esm"], false));
    }
    write(&tmp.path().join("F/textures/f.dds"), "f");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let all = ["SKSE", "Base", "Patch", "C", "D", "E", "F", "G"];
    for m in all {
        install::install(&inst, &tmp.path().join(m), None, true, None).unwrap();
    }
    rules::set_rule(&inst, &rules::ModRule { id: "G".into(), requires: vec!["C".into()], ..Default::default() }).unwrap();
    let suspects = bisect::suspects(&inst, true).unwrap();
    assert_eq!(suspects, ["Base", "Patch", "C", "D", "E", "F", "G"]);

    // The game crashes while D is on. A mod is never on without what it needs.
    let on = |inst: &Instance, id: &str| inst.profile().unwrap().mods.iter().any(|m| m.id == id && m.enabled);
    let mut games = 0;
    let crashes_with = |culprit: Option<&'static str>| {
        move |inst: &Instance| {
            assert!(!on(inst, "Patch") || on(inst, "Base"), "Patch is on without its master");
            assert!(!on(inst, "G") || on(inst, "C"), "G is on without C, which its rule requires");
            assert!(on(inst, "SKSE"));
            Ok((culprit.is_none_or(|c| !on(inst, c)), "a test".to_string()))
        }
    };
    let s = bisect::start(&inst, &suspects).unwrap();
    assert!(s.message.starts_with("Step 1:") && s.state.off.len() >= 3, "{}", s.message);
    let s = bisect::run(&inst, |i| {
        games += 1;
        crashes_with(Some("D"))(i)
    })
    .unwrap();
    assert_eq!(s.culprit.as_deref(), Some("D"), "{s:?}");
    assert!(s.message.starts_with("Found it: D."), "{}", s.message);
    assert!(games <= 4 && s.state.steps.len() == games, "{games} games");
    assert!(all.iter().all(|m| on(&inst, m)), "every step was undone");
    assert!(inst.profile().unwrap().plugins.iter().all(|p| p.enabled));
    assert!(!inst.history().unwrap().first().is_some_and(|l| l.starts_with("bisect")));
    assert!(bisect::start(&inst, &suspects).unwrap_err().to_string().contains("under way"));
    assert!(bisect::stop(&inst).unwrap().is_some() && bisect::status(&inst).unwrap().is_none());

    // A crash none of them causes: each half is ruled out, then the last one on its own.
    bisect::start(&inst, &suspects).unwrap();
    let s = bisect::run(&inst, crashes_with(Some("SKSE"))).unwrap();
    assert!(s.culprit.is_none() && s.state.left.is_empty() && s.message.starts_with("Not found"), "{s:?}");
    bisect::stop(&inst).unwrap();

    // By hand: one step with E off, which didn't crash.
    let s = bisect::start(&inst, &["D".into(), "E".into()]).unwrap();
    assert_eq!(s.state.off, ["E"]);
    assert!(!on(&inst, "E") && on(&inst, "D"));
    let s = bisect::mark(&inst, false).unwrap();
    assert_eq!(s.culprit.as_deref(), Some("E"));
    assert!(on(&inst, "E"));
    bisect::stop(&inst).unwrap();

    // Undone by hand already: fine. Another change since the step: refused, and stop says so.
    bisect::start(&inst, &["D".into(), "E".into()]).unwrap();
    inst.undo().unwrap();
    assert_eq!(bisect::mark(&inst, true).unwrap().state.off, ["D"]);
    ops::set_mods_enabled(&inst, &["F".into()], false).unwrap();
    assert!(bisect::mark(&inst, true).unwrap_err().to_string().contains("the setup changed"));
    let s = bisect::stop(&inst).unwrap().unwrap();
    assert!(s.message.contains("History: D"), "{}", s.message);
}

/// Majestic Mountains' installer names none of its steps: each must take its own choices.
#[test]
fn fomod_steps_without_names() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let src = tmp.path().join("MM");
    let xml = r#"<config><moduleName>MM</moduleName><installSteps>
      <installStep name=""><optionalFileGroups><group name="Landscape" type="SelectExactlyOne"><plugins>
        <plugin name="Non AE"><description/><files><file source="se.txt" destination="landscape.txt"/></files><typeDescriptor><type name="Recommended"/></typeDescriptor></plugin>
        <plugin name="AE"><description/><files><file source="ae.txt" destination="landscape.txt"/></files><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
      </plugins></group></optionalFileGroups></installStep>
      <installStep name=""><optionalFileGroups><group name="Final Notes" type="SelectAny"><plugins>
        <plugin name="Read me"><description/><files><file source="notes.txt" destination="notes.txt"/></files><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
      </plugins></group></optionalFileGroups></installStep>
    </installSteps></config>"#;
    write(&src.join("fomod/ModuleConfig.xml"), xml);
    for f in ["se.txt", "ae.txt", "notes.txt"] {
        write(&src.join(f), f);
    }
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();

    // The window sends back every step's choices, both called "".
    let walk = install::fomod_options(&inst, &src, None).unwrap();
    assert_eq!(walk.steps.len(), 2);
    let mut c = walk.choices.clone();
    c.options[0].groups[0].choices = vec![serde_json::from_value(serde_json::json!({"name": "AE"})).unwrap()];
    c.options[1].groups[0].choices = vec![serde_json::from_value(serde_json::json!({"name": "Read me"})).unwrap()];
    let w = install::fomod_options(&inst, &src, Some(&c)).unwrap();
    assert!(w.warnings.is_empty(), "{:?}", w.warnings);
    let picked: Vec<&str> = w.choices.options.iter().map(|s| s.groups[0].choices[0].name.as_str()).collect();
    assert_eq!(picked, ["AE", "Read me"]);
    let r = install::install(&inst, &src, None, true, Some(&c)).unwrap();
    let dir = inst.mod_dir(&r.id);
    assert_eq!(read(&dir.join("landscape.txt")), "ae.txt");
    assert_eq!(read(&dir.join("notes.txt")), "notes.txt");
}

/// Embers XD's Lanterns patch sorted before Embers XD.esp by name: a mod's plugins go in after
/// their masters from the same mod, so nothing loads before what it's built on.
#[test]
fn installs_plugins_after_their_masters() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&game.join("Data/Skyrim.esm"), plugin_bytes(&[], true));
    let src = tmp.path().join("Embers");
    write(&src.join("A Patch.esp"), plugin_bytes(&["Skyrim.esm", "Zed.esp"], false));
    write(&src.join("Zed.esp"), plugin_bytes(&["Skyrim.esm"], false));
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    install::install(&inst, &src, None, true, None).unwrap();
    let order: Vec<String> = inst.profile().unwrap().plugins.iter().map(|p| p.name.clone()).collect();
    assert_eq!(order, ["Zed.esp", "A Patch.esp"]);
    let issues = health::check(&inst).unwrap();
    assert!(!issues.iter().any(|i| i.kind == "missing_masters"), "{issues:?}");

    // Out of order with its master on: the finding says so instead of "needs it turned on".
    inst.edit("swap", |p| {
        p.plugins.reverse();
        Ok(())
    })
    .unwrap();
    let issues = health::check(&inst).unwrap();
    let m = issues.iter().find(|i| i.kind == "missing_masters").unwrap();
    assert!(m.message.starts_with("loads before Zed.esp, which it's built on"), "{}", m.message);
}

/// MfgFix and MfgFix MCM come from one Nexus page: a new file updates the mod whose file has its
/// name, and when no name settles it there's no offer, rather than the first mod of that page.
#[test]
fn update_offer_tells_files_of_one_page_apart() {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write(&tmp.path().join("game/SkyrimSE.exe"), "exe");
    write(&tmp.path().join("game/Data/Skyrim.esm"), plugin_bytes(&[], true));
    for d in ["MfgFix-11669-1-6-1700000000", "MfgFix MCM-11669-1-0-1700000000", "MfgFix MCM-11669-1-2-1710000000", "Extra-11669-1-0-1710000000"] {
        write(&src.join(d).join("x.txt"), d);
    }
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &tmp.path().join("game"), &tmp.path().join("local")).unwrap();
    let fix = install::install(&inst, &src.join("MfgFix-11669-1-6-1700000000"), None, true, None).unwrap().id;
    let new_mcm = src.join("MfgFix MCM-11669-1-2-1710000000");
    assert_eq!(install::same_mod(&inst, &new_mcm, None).unwrap().map(|m| m.0), Some(fix), "one mod from the page: offered as before");
    let mcm = install::install(&inst, &src.join("MfgFix MCM-11669-1-0-1700000000"), None, true, None).unwrap().id;
    assert_eq!(install::same_mod(&inst, &new_mcm, Some(11669)).unwrap().map(|m| m.0), Some(mcm));
    assert_eq!(install::same_mod(&inst, &src.join("Extra-11669-1-0-1710000000"), None).unwrap(), None);
}
