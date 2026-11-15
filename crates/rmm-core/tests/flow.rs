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
    let a = install::install(&inst, &src.join("A"), None, true).unwrap();
    let b = install::install(&inst, &zip_path, Some("B"), true).unwrap();
    let s = install::install(&inst, &src.join("skse"), Some("SKSE"), true).unwrap();
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

    // A tool replaces a deployed file (atomic save): captured to overwrite/, never lost.
    fs::remove_file(game.join("Data/meshes/a.nif")).unwrap();
    write(&game.join("Data/meshes/a.nif"), "edited by tool");
    assert!(health::check(&inst).unwrap().iter().any(|i| i.kind == "external_change"));
    deploy::deploy(&inst, false).unwrap();
    assert_eq!(read(&root.join("overwrite/meshes/a.nif")), "edited by tool");
    assert_eq!(read(&game.join("Data/meshes/a.nif")), "a-mesh");

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

#[test]
fn fomod_archives_are_refused_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    write(&game.join("SkyrimSE.exe"), "exe");
    write(&tmp.path().join("m/fomod/ModuleConfig.xml"), "<config/>");
    write(&tmp.path().join("m/opt1/textures/x.dds"), "x");
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("l")).unwrap();
    let err = install::install(&inst, &tmp.path().join("m"), None, true).unwrap_err();
    assert!(err.to_string().contains("FOMOD"));
    assert!(inst.profile().unwrap().mods.is_empty());
}
