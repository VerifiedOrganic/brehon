use std::path::Path;

use anyhow::Result;

pub fn execute(project_path: Option<&Path>, repair: bool, json: bool) -> Result<()> {
    let brehon_root = project_path
        .map(|p| p.join(".brehon"))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(".brehon"));

    if repair {
        let report = brehon_doctor::run_doctor_repair(&brehon_root);
        if json {
            println!("{}", report.to_json()?);
        } else {
            println!("{}", report);
        }
        return Ok(());
    }

    let (report_text, has_errors) = brehon_doctor::run_doctor_cli(&brehon_root);

    println!("{}", report_text);

    if has_errors {
        std::process::exit(1);
    }

    Ok(())
}
