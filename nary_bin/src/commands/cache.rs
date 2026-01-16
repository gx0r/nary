use bytesize::ByteSize;
use nary_lib::dir_size;

use crate::error::Result;
use crate::{CacheArgs, CacheCommands};

/// Manage the package cache
pub fn run_cache(args: &CacheArgs) -> Result<()> {
    match &args.command {
        CacheCommands::Clean => {
            let bytes = nary_lib::clear_cache()?;
            eprintln!("Cleared cache ({} freed)", ByteSize::b(bytes));
            Ok(())
        }
        CacheCommands::Ls => {
            let cache_dir = nary_lib::get_cache_dir()?;
            let size = dir_size(&cache_dir);
            eprintln!("Cache location: {}", cache_dir.display());
            eprintln!("Cache size: {}", ByteSize::b(size));
            Ok(())
        }
    }
}
