//! Spike runner: load the spike driver crate with rust-analyzer and evaluate
//! `__pliron_lsp_analyze` under r-a's MIR interpreter.
//!
//! Usage: pliron-lsp-spike-runner <path-to-driver-crate> [iterations]

use std::path::PathBuf;
use std::time::Instant;

use std::collections::BTreeMap;

use ra_ap_hir::{Adt, Crate, HirDisplay, ModuleDef};
use ra_ap_load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_at};
use ra_ap_project_model::{CargoConfig, RustLibSource};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("path to driver crate"));
    let iterations: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(2);
    let fn_names: Vec<String> = args.collect();
    let fn_names = if fn_names.is_empty() { vec!["__pliron_lsp_analyze".to_string()] } else { fn_names };

    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        all_targets: false,
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };

    let t = Instant::now();
    let (db, _vfs, _pm) = load_workspace_at(&path, &cargo_config, &load_config, &|msg| {
        eprintln!("[load] {msg}")
    })?;
    eprintln!("workspace loaded in {:?}", t.elapsed());

    if fn_names.first().map(|s| s.as_str()) == Some("--diag") {
        ra_ap_hir_ty::attach_db(&db, || diag(&db, &fn_names[1..]));
        return Ok(());
    }

    let krate = Crate::all(&db)
        .into_iter()
        .find(|k| {
            k.display_name(&db)
                .is_some_and(|n| n.to_string() == "pliron_lsp_spike_driver")
        })
        .expect("driver crate not found");
    for fn_name in &fn_names {
    let func = krate
        .root_module(&db)
        .declarations(&db)
        .into_iter()
        .find_map(|d| match d {
            ModuleDef::Function(f) if f.name(&db).as_str() == fn_name => Some(f),
            _ => None,
        })
        .expect("driver fn not found");
    println!("##### {fn_name}");
    for i in 0..iterations {
        let t = Instant::now();
        let res = ra_ap_hir_ty::attach_db(&db, || {
            func.eval(&db, |file, range| format!("{file:?}:{range:?}"))
        });
        let elapsed = t.elapsed();
        match res {
            Ok(text) => println!("=== eval #{i} ({elapsed:?}) ===\n{text}"),
            Err(e) => println!("=== eval #{i} ({elapsed:?}) FAILED ===\n{e:?}"),
        }
    }
    }
    Ok(())
}

fn diag(db: &ra_ap_ide_db::RootDatabase, crates: &[String]) {
    for name in crates {
        let Some(krate) = Crate::all(db)
            .into_iter()
            .find(|k| k.display_name(db).is_some_and(|n| n.to_string() == *name))
        else {
            println!("crate {name} not found");
            continue;
        };
        let target = krate.to_display_target(db);
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut unknown_fields = Vec::new();
        for module in krate.modules(db) {
            let mut acc = Vec::new();
            module.diagnostics(db, &mut acc, false);
            for d in &acc {
                let dbg = format!("{d:?}");
                let kind = dbg.split(['(', ' ', '{']).next().unwrap_or("?").to_string();
                if kind == "UnresolvedMacroCall" {
                    if let Some(i) = dbg.find("path:") {
                        let tail: String = dbg[i..].chars().take(160).collect();
                        *counts.entry(format!("UMC {}", tail.split(", is_bang").next().unwrap_or(&tail))).or_default() += 1;
                    }
                }
                *counts.entry(kind).or_default() += 1;
            }
            for decl in module.declarations(db) {
                if let ModuleDef::Adt(Adt::Struct(st)) = decl {
                    for f in st.fields(db) {
                        let ty = f.ty(db);
                        if ty.contains_unknown() {
                            unknown_fields.push(format!(
                                "{}::{} : {}",
                                st.name(db).as_str(),
                                f.name(db).as_str(),
                                ty.display(db, target)
                            ));
                        }
                    }
                }
            }
        }
        println!("== crate {name}: {} modules", krate.modules(db).len());
        for (k, n) in &counts {
            println!("  diag {k}: {n}");
        }
        for u in unknown_fields.iter().take(30) {
            println!("  unknown field type: {u}");
        }
        println!("  ({} struct fields with unknown types)", unknown_fields.len());
    }
}
