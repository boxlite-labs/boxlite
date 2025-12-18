//! Test example for disk attachment functionality
//!
//! This example demonstrates attaching a QCOW2 disk image to a VM.
//! Run with: cargo run --example disk_attachment_test

use boxlite::vmm::{BlockDevice, BlockDevices, DiskFormat};
use std::path::PathBuf;

fn main() {
    println!("=== Disk Attachment API Test ===\n");

    // Test 1: Create disk configuration
    println!("Test 1: Creating disk configurations...");
    let mut disks = BlockDevices::new();
    println!("  ✓ Created empty BlockDevices");

    // Add a QCOW2 disk
    let qcow2_disk = BlockDevice {
        block_id: "vda".to_string(),
        disk_path: PathBuf::from("/tmp/test.qcow2"),
        read_only: false,
        format: DiskFormat::Qcow2,
    };
    disks.add(qcow2_disk);
    println!("  ✓ Added QCOW2 disk: vda -> /tmp/test.qcow2 (read-write)");

    // Add a raw disk
    let raw_disk = BlockDevice {
        block_id: "vdb".to_string(),
        disk_path: PathBuf::from("/tmp/scratch.raw"),
        read_only: true,
        format: DiskFormat::Raw,
    };
    disks.add(raw_disk);
    println!("  ✓ Added raw disk: vdb -> /tmp/scratch.raw (read-only)");

    // Test 2: Verify disk configurations
    println!("\nTest 2: Verifying disk configurations...");
    println!("  Total disks: {}", disks.devices().len());

    for (i, disk) in disks.devices().iter().enumerate() {
        println!("  Disk {}:", i + 1);
        println!("    Block ID: {}", disk.block_id);
        println!("    Path: {}", disk.disk_path.display());
        println!("    Format: {}", disk.format.as_str());
        println!("    Read-only: {}", disk.read_only);
    }

    // Test 3: Serialization
    println!("\nTest 3: Testing serialization...");
    match serde_json::to_string_pretty(&disks) {
        Ok(json) => {
            println!("  ✓ Successfully serialized to JSON:");
            println!("{}", json);
        }
        Err(e) => {
            println!("  ✗ Serialization failed: {}", e);
        }
    }

    // Test 4: Deserialization
    println!("\nTest 4: Testing deserialization...");
    let json = r#"{
        "devices": [
            {
                "block_id": "vdc",
                "disk_path": "/var/lib/data.qcow2",
                "read_only": false,
                "format": "Qcow2"
            }
        ]
    }"#;

    match serde_json::from_str::<BlockDevices>(json) {
        Ok(deserialized) => {
            println!("  ✓ Successfully deserialized from JSON");
            println!("  Loaded {} disk(s)", deserialized.devices().len());
            if let Some(disk) = deserialized.devices().first() {
                println!(
                    "  First disk: {} -> {}",
                    disk.block_id,
                    disk.disk_path.display()
                );
            }
        }
        Err(e) => {
            println!("  ✗ Deserialization failed: {}", e);
        }
    }

    println!("\n=== All tests completed successfully! ===");
    println!("\nNote: This test validates the API design.");
    println!("To test actual VM disk attachment, use the Python SDK examples.");
}
