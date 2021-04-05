#![warn(missing_docs)]

use crate::architecture::{
    arm::{
        communication_interface::{
            ApInformation::{MemoryAp, Other},
            ArmProbeInterface, MemoryApInformation,
        },
        core::{reset_catch_clear, reset_catch_set},
        memory::Component,
        SwoConfig,
    },
    riscv::communication_interface::RiscvCommunicationInterface,
};
use crate::config::{Architecture, ChipInfo, MemoryRegion, RegistryError, Target, TargetSelector};
use crate::core::{CoreState, SpecificCoreState};
use crate::{architecture::arm::ap::MemoryAP, config::DebugSequence};
use crate::{AttachMethod, Core, CoreType, Error, Probe};
use anyhow::anyhow;
use std::{
    borrow::{Borrow, BorrowMut},
    time::Duration,
};

/// The `Session` struct represents an active debug session.
///
/// ## Creating a session  
/// The session can be created by calling the [Session::auto_attach()] function,
/// which tries to automatically select a probe, and then connect to the target.  
///
/// For more control, the [Probe::attach()] and [Probe::attach_under_reset()]
/// methods can be used to open a `Session` from a specific [Probe].  
///
/// # Usage  
/// The Session is the common handle that gives a user exclusive access to an active probe.  
/// You can create and share a session between threads to enable multiple stakeholders (e.g. GDB and RTT) to access the target taking turns, by using  `Arc<Mutex<Session>>.`  
///
/// If you do so, make sure that both threads sleep in between tasks such that other stakeholders may take their turn.  
///
/// To get access to a single [Core] from the `Session`, the [Session::core()] method can be used.
/// Please see the [Session::core()] method for more usage guidelines.
///

#[derive(Debug)]
pub struct Session {
    target: Target,
    interface: ArchitectureInterface,
    cores: Vec<(SpecificCoreState, CoreState)>,
}

#[derive(Debug)]
enum ArchitectureInterface {
    Arm(Box<dyn ArmProbeInterface + 'static>),
    Riscv(Box<RiscvCommunicationInterface>),
}

impl From<ArchitectureInterface> for Architecture {
    fn from(value: ArchitectureInterface) -> Self {
        match value {
            ArchitectureInterface::Arm(_) => Architecture::Arm,
            ArchitectureInterface::Riscv(_) => Architecture::Riscv,
        }
    }
}

impl ArchitectureInterface {
    fn attach<'probe>(
        &'probe mut self,
        core: &'probe mut SpecificCoreState,
        core_state: &'probe mut CoreState,
    ) -> Result<Core<'probe>, Error> {
        match self {
            ArchitectureInterface::Arm(state) => {
                let memory = state.memory_interface(0.into())?;

                core.attach_arm(core_state, memory)
            }
            ArchitectureInterface::Riscv(state) => core.attach_riscv(core_state, state),
        }
    }

    /// Deassert the target reset line
    ///
    /// When connecting under reset,
    /// initial configuration is done with the reset line
    /// asserted. After initial configuration is done, the
    /// reset line can be deasserted using this method.
    ///
    /// See also [`Probe::target_reset_deassert`].
    fn target_reset_deassert(&mut self) -> Result<(), Error> {
        match self {
            ArchitectureInterface::Arm(arm_interface) => arm_interface.target_reset_deassert()?,

            ArchitectureInterface::Riscv(riscv_interface) => {
                riscv_interface.target_reset_deassert()?
            }
        }

        Ok(())
    }
}

impl Session {
    /// Open a new session with a given debug target.
    pub(crate) fn new(
        probe: Probe,
        target: impl Into<TargetSelector>,
        attach_method: AttachMethod,
    ) -> Result<Self, Error> {
        let (probe, target) = get_target_from_selector(target, probe)?;

        let mut session = match target.architecture() {
            Architecture::Arm => {
                let mut interface = probe.try_into_arm_interface().map_err(|(_, err)| err)?;

                match target.debug_sequence.borrow() {
                    DebugSequence::Arm(sequence) => {
                        sequence.debug_port_setup(interface.borrow_mut())?
                    }
                    DebugSequence::Riscv => panic!("Should not happen...."),
                }

                let mut interface = interface.initialize()?;

                {
                    let mut memory_interface = interface.memory_interface(MemoryAP::from(0))?;

                    // Enable debug mode
                    match target.debug_sequence.borrow() {
                        DebugSequence::Arm(sequence) => {
                            sequence.debug_core_start(&mut memory_interface)?
                        }
                        DebugSequence::Riscv => panic!("Should not happen...."),
                    }
                }

                let core = (
                    SpecificCoreState::from_core_type(target.core_type),
                    Core::create_state(0),
                );

                let mut session = Session {
                    target,
                    interface: ArchitectureInterface::Arm(interface),
                    cores: vec![core],
                };

                if attach_method == AttachMethod::UnderReset {
                    // we need to halt the chip here
                    reset_catch_set(&mut session.core(0)?)?;

                    // Deassert the reset pin
                    session.interface.target_reset_deassert()?;

                    // Wait for the core to be halted
                    let mut core = session.core(0)?;

                    core.wait_for_core_halted(Duration::from_millis(100))?;

                    reset_catch_clear(&mut core)?;
                }

                session
            }
            Architecture::Riscv => {
                // TODO: Handle attach under reset

                let core = (
                    SpecificCoreState::from_core_type(target.core_type),
                    Core::create_state(0),
                );

                let interface = probe
                    .try_into_riscv_interface()
                    .map_err(|(_probe, err)| err)?;

                let mut session = Session {
                    target,
                    interface: ArchitectureInterface::Riscv(Box::new(interface)),
                    cores: vec![core],
                };

                {
                    let mut core = session.core(0)?;

                    core.halt(Duration::from_millis(100))?;
                }

                session
            }
        };

        session.clear_all_hw_breakpoints()?;

        Ok(session)
    }

    /// Automatically creates a session with the first connected probe found.
    pub fn auto_attach(target: impl Into<TargetSelector>) -> Result<Session, Error> {
        // Get a list of all available debug probes.
        let probes = Probe::list_all();

        // Use the first probe found.
        let probe = probes
            .get(0)
            .ok_or(Error::UnableToOpenProbe("No probe was found"))?
            .open()?;

        // Attach to a chip.
        probe.attach(target)
    }

    /// Lists the available cores with their number and their type.
    pub fn list_cores(&self) -> Vec<(usize, CoreType)> {
        self.cores
            .iter()
            .map(|(t, _)| t.core_type())
            .enumerate()
            .collect()
    }

    /// Attaches to the core with the given number.
    ///
    /// ## Usage
    /// Everytime you want to perform an operation on the chip, you need to get the Core handle with the [Session::core() method. This [Core] handle is merely a view into the core. And provides a convenient API surface.
    ///
    /// All the state is stored in the [Session] handle.
    ///
    /// The first time you call [Session::core()] for a specific core, it will run the attach/init sequences and return a handle to the [Core].
    ///
    /// Every subsequent call is a no-op. It simply returns the handle for the user to use in further operations without calling any int sequences again.
    ///
    /// It is strongly advised to never store the [Core] handle for any significant duration! Free it as fast as possible such that other stakeholders can have access to the [Core] too.
    ///
    /// The idea behind this is: You need the smallest common denominator which you can share between threads. Since you sometimes need the [Core], sometimes the [Probe] or sometimes the [Target], the [Session] is the only common ground and the only handle you should actively store in your code.
    ///
    pub fn core(&mut self, n: usize) -> Result<Core<'_>, Error> {
        let (core, core_state) = self.cores.get_mut(n).ok_or(Error::CoreNotFound(n))?;

        self.interface.attach(core, core_state)
    }

    /// Read available data from the SWO interface without waiting.
    ///
    /// This method is only supported for ARM-based targets, and will
    /// return [Error::ArchitectureRequired] otherwise.
    pub fn read_swo(&mut self) -> Result<Vec<u8>, Error> {
        let interface = self.get_arm_interface()?;
        interface.read_swo()
    }

    fn get_arm_interface(&mut self) -> Result<&mut Box<dyn ArmProbeInterface>, Error> {
        let interface = match &mut self.interface {
            ArchitectureInterface::Arm(state) => state,
            _ => return Err(Error::ArchitectureRequired(&["ARMv7", "ARMv8"])),
        };

        Ok(interface)
    }

    /// Reads all the available ARM CoresightComponents of the currently attached target.
    ///
    /// This will recursively parse the Romtable of the attached target
    /// and create a list of all the contained components.
    pub fn get_arm_components(&mut self) -> Result<Vec<Component>, Error> {
        let interface = self.get_arm_interface()?;

        let mut components = Vec::new();

        for ap_index in 0..(interface.num_access_ports() as u8) {
            let ap_information = interface
                .ap_information(ap_index.into())
                .ok_or_else(|| anyhow!("AP {} does not exist on chip.", ap_index))?;

            let component = match ap_information {
                MemoryAp(MemoryApInformation {
                    port_number: _,
                    only_32bit_data_size: _,
                    debug_base_address: 0,
                    supports_hnonsec: _,
                }) => Err(Error::Other(anyhow!("AP has a base address of 0"))),
                MemoryAp(MemoryApInformation {
                    port_number,
                    only_32bit_data_size: _,
                    debug_base_address,
                    supports_hnonsec: _,
                }) => {
                    let access_port_number = *port_number;
                    let base_address = *debug_base_address;

                    let mut memory = interface.memory_interface(access_port_number.into())?;

                    Component::try_parse(&mut memory, base_address)
                        .map_err(Error::architecture_specific)
                }
                Other { port_number } => {
                    // Return an error, only possible to get Component from MemoryAP
                    Err(Error::Other(anyhow!(
                        "AP {} is not a MemoryAP, unable to get ARM component.",
                        port_number
                    )))
                }
            };

            match component {
                Ok(component) => {
                    components.push(component);
                }
                Err(e) => {
                    log::info!("Not counting AP {} because of: {}", ap_index, e);
                }
            }
        }

        Ok(components)
    }

    /// Get the target description of the connected target.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Configure the target and probe for serial wire view (SWV) tracing.
    pub fn setup_swv(&mut self, config: &SwoConfig) -> Result<(), Error> {
        // Configure SWO on the probe
        {
            let interface = self.get_arm_interface()?;
            interface.enable_swo(config)?;
        }

        // Enable tracing on the target
        {
            let mut core = self.core(0)?;
            crate::architecture::arm::component::enable_tracing(&mut core)?;
        }

        // Configure SWV on the target
        let components = self.get_arm_components()?;
        let mut core = self.core(0)?;
        crate::architecture::arm::component::setup_swv(&mut core, &components, config)
    }

    /// Configure the target to stop emitting SWV trace data.
    pub fn disable_swv(&mut self) -> Result<(), Error> {
        crate::architecture::arm::component::disable_swv(&mut self.core(0)?)
    }

    /// Begin tracing a memory address over SWV.
    pub fn add_swv_data_trace(&mut self, unit: usize, address: u32) -> Result<(), Error> {
        let components = self.get_arm_components()?;
        let mut core = self.core(0)?;
        crate::architecture::arm::component::add_swv_data_trace(
            &mut core,
            &components,
            unit,
            address,
        )
    }

    /// Stop tracing from a given SWV unit
    pub fn remove_swv_data_trace(&mut self, unit: usize) -> Result<(), Error> {
        let components = self.get_arm_components()?;
        let mut core = self.core(0)?;
        crate::architecture::arm::component::remove_swv_data_trace(&mut core, &components, unit)
    }

    /// Returns the memory map of the target.
    #[deprecated = "Use the Session::target function instead"]
    pub fn memory_map(&self) -> &[MemoryRegion] {
        &self.target.memory_map
    }

    /// Return the `Architecture` of the currently connected chip.
    pub fn architecture(&self) -> Architecture {
        match self.interface {
            ArchitectureInterface::Arm(_) => Architecture::Arm,
            ArchitectureInterface::Riscv(_) => Architecture::Riscv,
        }
    }

    /// Clears all hardware breakpoints on all cores
    pub fn clear_all_hw_breakpoints(&mut self) -> Result<(), Error> {
        { 0..self.cores.len() }.try_for_each(|n| {
            self.core(n)
                .and_then(|mut core| core.clear_all_hw_breakpoints())
        })
    }
}

// This test ensures that [Session] is fully [Send] + [Sync].
static_assertions::assert_impl_all!(Session: Send);

impl Drop for Session {
    fn drop(&mut self) {
        let result = { 0..self.cores.len() }.try_for_each(|i| {
            self.core(i)
                .and_then(|mut core| core.clear_all_set_hw_breakpoints())
        });

        if let Err(err) = result {
            log::warn!("Could not clear all hardware breakpoints: {:?}", err);
        }
    }
}
/// Determine the [Target] from a [TargetSelector].
///
/// If the selector is [TargetSelector::Unspecified], the target will be looked up in the registry.
/// If it its [TargetSelector::Auto], probe-rs will try to determine the target automatically, based on
/// information read from the chip.
fn get_target_from_selector(
    target: impl Into<TargetSelector>,
    probe: Probe,
) -> Result<(Probe, Target), Error> {
    let mut probe = probe;

    let target = match target.into() {
        TargetSelector::Unspecified(name) => crate::config::get_target_by_name(name)?,
        TargetSelector::Specified(target) => target,
        TargetSelector::Auto => {
            let mut found_chip = None;

            if probe.has_arm_interface() {
                match probe.try_into_arm_interface() {
                    Ok(interface) => {
                        let mut interface = interface.initialize()?;

                        //let chip_result = try_arm_autodetect(interface);
                        log::debug!("Autodetect: Trying DAP interface...");

                        let found_arm_chip = interface.read_from_rom_table().unwrap_or_else(|e| {
                            log::info!("Error during auto-detection of ARM chips: {}", e);
                            None
                        });

                        found_chip = found_arm_chip.map(ChipInfo::from);

                        probe = interface.close();
                    }
                    Err((returned_probe, err)) => {
                        probe = returned_probe;
                        log::debug!("Error using ARM interface: {}", err);
                    }
                }
            } else {
                log::debug!("No ARM interface was present. Skipping Riscv autodetect.");
            }

            if found_chip.is_none() && probe.has_riscv_interface() {
                match probe.try_into_riscv_interface() {
                    Ok(mut interface) => {
                        let idcode = interface.read_idcode();

                        log::debug!("ID Code read over JTAG: {:x?}", idcode);

                        probe = interface.close();
                    }
                    Err((returned_probe, err)) => {
                        log::debug!("Error during autodetection of RISCV chips: {}", err);
                        probe = returned_probe;
                    }
                }
            } else {
                log::debug!("No RISCV interface was present. Skipping Riscv autodetect.");
            }

            if let Some(chip) = found_chip {
                crate::config::get_target_by_chip_info(chip)?
            } else {
                return Err(Error::ChipNotFound(RegistryError::ChipAutodetectFailed));
            }
        }
    };

    Ok((probe, target))
}
