#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(flag) = args.iter().position(|a| a == "--agent-hook") {
        // Spawned by coding agents; the source defaults to `zcode` when omitted.
        let source = args.get(flag + 1).map(String::as_str).unwrap_or("zcode");
        kuro_cat_pet_lib::run_agent_hook(source);
    }
    if args.iter().any(|a| a == "--mcp") {
        kuro_cat_pet_lib::run_mcp_stdio();
        return;
    }
    if args.iter().any(|a| a == "--hook") {
        kuro_cat_pet_lib::run_hook_cli(&args);
        return;
    }
    kuro_cat_pet_lib::run();
}
