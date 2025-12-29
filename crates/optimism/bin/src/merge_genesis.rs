use alloy_genesis::Genesis;
use alloy_primitives::Address;
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use eyre::Result;

fn main() -> Result<()> {
    // Get the genesis.json file path from command line arguments
    let genesis_json_path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| eyre::eyre!("Usage: merge_genesis <genesis.json> <genesis_random.json>"))?;
    
    // Get the genesis_random.json file path from command line arguments
    let genesis_random_json_path = env::args()
        .nth(2)
        .map(PathBuf::from)
        .ok_or_else(|| eyre::eyre!("Usage: merge_genesis <genesis.json> <genesis_random.json> <output.json>"))?;
    
    // Get the output file path from command line arguments
    let merged_genesis_json_path = env::args()
        .nth(3)
        .map(PathBuf::from)
        .ok_or_else(|| eyre::eyre!("Usage: merge_genesis <genesis.json> <genesis_random.json> <output.json>"))?;
    
    // Read the base genesis.json file
    let genesis_json_content = std::fs::read_to_string(&genesis_json_path)?;
    let mut base_genesis: Genesis = serde_json::from_str(&genesis_json_content)?;
    
    println!("Loaded base genesis from {}", genesis_json_path.display());
    
    // Read the genesis_random.json file
    let genesis_random_json_content = std::fs::read_to_string(&genesis_random_json_path)?;
    let random_genesis: Genesis = serde_json::from_str(&genesis_random_json_content)?;
    
    println!("Loaded random genesis from {}", genesis_random_json_path.display());
    
    // Get the alloc from random_genesis
    let random_alloc = random_genesis.alloc;
    
    if random_alloc.is_empty() {
        println!("Warning: genesis_random.json has no accounts in alloc");
    }
    
    // Merge alloc: use random_genesis alloc to replace or insert into base_genesis alloc
    let mut merged_count = 0;
    let mut replaced_count = 0;
    
    {
        let base_alloc = &mut base_genesis.alloc;
        
        for (address, account) in random_alloc {
            if base_alloc.contains_key(&address) {
                replaced_count += 1;
            } else {
                merged_count += 1;
            }
            base_alloc.insert(address, account);
        }
    }
    
    println!("Merged {} new accounts, replaced {} existing accounts", merged_count, replaced_count);
    
    // Write the merged genesis to output file
    let json_string = serde_json::to_string_pretty(&base_genesis)?;
    std::fs::write(&merged_genesis_json_path, json_string)?;
    
    println!("Written merged genesis to {}", merged_genesis_json_path.display());
    println!("Total accounts in merged genesis: {}", base_genesis.alloc.len());
    
    Ok(())
}

