//! Output formatting utilities.

use anyhow::Result;
use colored::Colorize;
use serde::Serialize;
use tabled::{Table, Tabled};

/// Format output based on format type
pub fn format_output<T: Serialize>(data: &T, format: super::OutputFormat) -> Result<String> {
    match format {
        super::OutputFormat::Json => Ok(serde_json::to_string_pretty(data)?),
        super::OutputFormat::Yaml => {
            // For simplicity, using JSON format for YAML too
            // TODO: Add serde_yaml dependency for proper YAML output
            Ok(serde_json::to_string_pretty(data)?)
        }
        super::OutputFormat::Table => {
            // Table format handled by specific functions
            Ok(serde_json::to_string_pretty(data)?)
        }
    }
}

/// Print success message
pub fn print_success(message: &str) {
    println!("{} {}", "✓".green().bold(), message);
}

/// Print error message
pub fn print_error(message: &str) {
    eprintln!("{} {}", "✗".red().bold(), message);
}

/// Print warning message
pub fn print_warning(message: &str) {
    println!("{} {}", "⚠".yellow().bold(), message);
}

/// Print info message
pub fn print_info(message: &str) {
    println!("{} {}", "ℹ".blue().bold(), message);
}

/// Print table
pub fn print_table<T: Tabled>(items: Vec<T>) {
    if items.is_empty() {
        print_info("No items found");
        return;
    }
    
    let table = Table::new(items);
    println!("{}", table);
}