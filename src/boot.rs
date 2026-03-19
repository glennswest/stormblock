//! Boot volume manager — templates, COW snapshots per machine, iPXE script generation.
//!
//! Server-side management of boot volumes. Template volumes are created and imaged,
//! then per-machine COW snapshots are provisioned for iSCSI sanboot.

use std::collections::HashMap;

use serde::{Serialize, Deserialize};

use crate::raid::RaidArrayId;
use crate::volume::{VolumeId, VolumeManager};

/// A boot template (golden image volume).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootTemplate {
    pub name: String,
    pub volume_id: VolumeId,
    pub size_bytes: u64,
    pub created: u64,
}

/// A provisioned machine instance (COW snapshot of a template).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineInstance {
    pub machine_id: String,
    pub template_name: String,
    pub volume_id: VolumeId,
    pub created: u64,
}

/// Boot volume manager: templates and per-machine instances.
pub struct BootManager {
    templates: HashMap<String, BootTemplate>,
    instances: HashMap<String, MachineInstance>,
    server_addr: String,
}

impl BootManager {
    /// Create a new boot manager.
    pub fn new(server_addr: &str) -> Self {
        BootManager {
            templates: HashMap::new(),
            instances: HashMap::new(),
            server_addr: server_addr.to_string(),
        }
    }

    /// Create a template volume for boot imaging.
    pub async fn create_template(
        &mut self,
        name: &str,
        size: u64,
        array_id: RaidArrayId,
        vm: &mut VolumeManager,
    ) -> anyhow::Result<VolumeId> {
        if self.templates.contains_key(name) {
            anyhow::bail!("template '{}' already exists", name);
        }

        let vol_name = format!("boot-template-{}", name);
        let vol_id = vm.create_volume(&vol_name, size, array_id).await
            .map_err(|e| anyhow::anyhow!("failed to create template volume: {e}"))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let template = BootTemplate {
            name: name.to_string(),
            volume_id: vol_id,
            size_bytes: size,
            created: now,
        };
        self.templates.insert(name.to_string(), template);

        Ok(vol_id)
    }

    /// Provision a machine from a template (COW snapshot).
    pub async fn provision_machine(
        &mut self,
        template_name: &str,
        machine_id: &str,
        vm: &mut VolumeManager,
    ) -> anyhow::Result<VolumeId> {
        let template = self.templates.get(template_name)
            .ok_or_else(|| anyhow::anyhow!("template '{}' not found", template_name))?;

        if self.instances.contains_key(machine_id) {
            anyhow::bail!("machine '{}' already provisioned", machine_id);
        }

        let snap_name = format!("boot-{}", machine_id);
        let snap_id = vm.create_snapshot(template.volume_id, &snap_name).await
            .map_err(|e| anyhow::anyhow!("failed to create boot snapshot: {e}"))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let instance = MachineInstance {
            machine_id: machine_id.to_string(),
            template_name: template_name.to_string(),
            volume_id: snap_id,
            created: now,
        };
        self.instances.insert(machine_id.to_string(), instance);

        Ok(snap_id)
    }

    /// Deprovision a machine (delete its boot snapshot).
    pub async fn deprovision_machine(
        &mut self,
        machine_id: &str,
        vm: &mut VolumeManager,
    ) -> anyhow::Result<()> {
        let instance = self.instances.remove(machine_id)
            .ok_or_else(|| anyhow::anyhow!("machine '{}' not found", machine_id))?;

        vm.delete_volume(instance.volume_id).await
            .map_err(|e| anyhow::anyhow!("failed to delete boot volume: {e}"))?;

        Ok(())
    }

    /// List all templates.
    pub fn list_templates(&self) -> Vec<&BootTemplate> {
        self.templates.values().collect()
    }

    /// List all provisioned machines.
    pub fn list_machines(&self) -> Vec<&MachineInstance> {
        self.instances.values().collect()
    }

    /// Get a machine instance by ID.
    pub fn get_machine(&self, machine_id: &str) -> Option<&MachineInstance> {
        self.instances.get(machine_id)
    }

    /// Generate an iPXE script for a machine.
    pub fn ipxe_script(&self, machine_id: &str) -> Option<String> {
        let instance = self.instances.get(machine_id)?;
        let _ = instance; // used for IQN generation
        Some(format!(
            "#!ipxe\nsanboot iscsi:{}:::1:iqn.2024.io.stormblock:{}\n",
            self.server_addr, machine_id,
        ))
    }

    /// Get the volume ID for a machine's boot volume.
    pub fn machine_volume_id(&self, machine_id: &str) -> Option<VolumeId> {
        self.instances.get(machine_id).map(|i| i.volume_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipxe_script_generation() {
        let mut bm = BootManager::new("192.168.1.50");
        // Manually insert an instance for testing
        bm.instances.insert("test-machine".to_string(), MachineInstance {
            machine_id: "test-machine".to_string(),
            template_name: "ubuntu".to_string(),
            volume_id: VolumeId(Uuid::new_v4()),
            created: 0,
        });

        let script = bm.ipxe_script("test-machine").unwrap();
        assert!(script.contains("#!ipxe"));
        assert!(script.contains("sanboot iscsi:192.168.1.50"));
        assert!(script.contains("iqn.2024.io.stormblock:test-machine"));
    }

    #[test]
    fn boot_manager_templates_and_machines() {
        let bm = BootManager::new("192.168.1.50");
        assert!(bm.list_templates().is_empty());
        assert!(bm.list_machines().is_empty());
        assert!(bm.get_machine("nonexistent").is_none());
        assert!(bm.ipxe_script("nonexistent").is_none());
    }
}
