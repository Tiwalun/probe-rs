use std::sync::Arc;

use super::chip::Chip;
use super::flash_algorithm::RawFlashAlgorithm;
use super::memory::MemoryRegion;
use crate::{
    architecture::arm::{sequences::DefaultArmSequence, ArmCommunicationInterface},
    core::{Architecture, CoreType},
    DebugProbeError, Error, Memory,
};

/// This describes a complete target with a fixed chip model and variant.
#[derive(Clone)]
pub struct Target {
    /// The name of the target.
    pub name: String,
    /// The name of the flash algorithm.
    pub flash_algorithms: Vec<RawFlashAlgorithm>,
    /// The core type.
    pub core_type: CoreType,
    /// The memory map of the target.
    pub memory_map: Vec<MemoryRegion>,

    debug_sequence: Arc<DebugSequence>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Target {{
            identifier: {:?},
            flash_algorithms: {:?},
            memory_map: {:?},
        }}",
            self.name, self.flash_algorithms, self.memory_map
        )
    }
}

/// An error occured while parsing the target description.
pub type TargetParseError = serde_yaml::Error;

impl Target {
    /// Create a new target
    pub fn new(
        chip: &Chip,
        flash_algorithms: Vec<RawFlashAlgorithm>,
        core_type: CoreType,
    ) -> Target {
        let debug_sequence = match core_type.architecture() {
            Architecture::Arm => DebugSequence::Arm(Box::new(DefaultArmSequence {})),
            Architecture::Riscv => DebugSequence::Riscv,
        };

        Target {
            name: chip.name.clone().into_owned(),
            flash_algorithms,
            core_type,
            memory_map: chip.memory_map.clone().into_owned(),
            debug_sequence: Arc::new(DebugSequence::Riscv),
        }
    }

    /// Get the architectre of the target
    pub fn architecture(&self) -> Architecture {
        self.core_type.architecture()
    }
}

/// Selector for the debug target.
#[derive(Debug, Clone)]
pub enum TargetSelector {
    /// Specify the name of a target, which will
    /// be used to search the internal list of
    /// targets.
    Unspecified(String),
    /// Directly specify a target.
    Specified(Target),
    /// Try to automatically identify the target,
    /// by reading identifying information from
    /// the probe and / or target.
    Auto,
}

impl From<&str> for TargetSelector {
    fn from(value: &str) -> Self {
        TargetSelector::Unspecified(value.into())
    }
}

impl From<&String> for TargetSelector {
    fn from(value: &String) -> Self {
        TargetSelector::Unspecified(value.into())
    }
}

impl From<String> for TargetSelector {
    fn from(value: String) -> Self {
        TargetSelector::Unspecified(value)
    }
}

impl From<()> for TargetSelector {
    fn from(_value: ()) -> Self {
        TargetSelector::Auto
    }
}

impl From<Target> for TargetSelector {
    fn from(target: Target) -> Self {
        TargetSelector::Specified(target)
    }
}

pub enum DebugSequence {
    Arm(Box<dyn ArmDebugSequence>),
    Riscv,
}

pub trait ArmDebugSequence: Send + Sync {
    fn reset_hardware_assert(&self, interface: &mut Memory) -> Result<(), Error>;
    fn reset_hardware_deassert(&self, interface: &mut Memory) -> Result<(), Error>;

    fn debug_port_setup(&self, interface: &mut Memory) -> Result<(), Error>;

    fn debug_port_start(&self, interface: &mut Memory) -> Result<(), Error>;

    fn debug_device_unlock(&self, interface: &mut Memory) -> Result<(), Error> {
        // Empty by default
        Ok(())
    }

    fn debug_core_start(&self, interface: &mut Memory) -> Result<(), Error>;

    fn recover_support_start(&self, _interface: &mut Memory) -> Result<(), Error> {
        // Empty by default
        Ok(())
    }

    fn reset_catch_set(&self, interface: &mut Memory) -> Result<(), Error>;

    fn reset_catch_clear(&self, interface: &mut Memory) -> Result<(), Error>;

    fn reset_system(&self, interface: &mut Memory) -> Result<(), Error>;
}
