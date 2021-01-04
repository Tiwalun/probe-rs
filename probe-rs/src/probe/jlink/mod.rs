//! Support for J-Link Debug probes

use jaylink::{CommunicationSpeed, Interface, JayLink};
use thiserror::Error;

use std::convert::{TryFrom, TryInto};
use std::iter;

use crate::{
    architecture::arm::{DapError, PortType, Register},
    architecture::{
        arm::{
            communication_interface::ArmProbeInterface,
            dp::Abort,
            dp::{Ctrl, RdBuff},
            swo::SwoConfig,
            ArmCommunicationInterface, SwoAccess,
        },
        riscv::communication_interface::RiscvCommunicationInterface,
    },
    probe::{
        DAPAccess, DebugProbe, DebugProbeError, DebugProbeInfo, DebugProbeType, JTAGAccess,
        WireProtocol,
    },
    DebugProbeSelector, Error as ProbeRsError,
};

const SWO_BUFFER_SIZE: u16 = 128;

#[derive(Debug)]
pub(crate) struct JLink {
    handle: JayLink,
    swo_config: Option<SwoConfig>,

    /// Idle cycles necessary between consecutive
    /// accesses to the DMI register
    jtag_idle_cycles: u8,

    /// Currently selected protocol
    protocol: Option<WireProtocol>,

    /// Protocols supported by the connected J-Link probe.
    supported_protocols: Vec<WireProtocol>,

    current_ir_reg: u32,

    speed_khz: u32,
}

impl JLink {
    fn idle_cycles(&self) -> u8 {
        self.jtag_idle_cycles
    }

    fn select_interface(
        &mut self,
        protocol: Option<WireProtocol>,
    ) -> Result<WireProtocol, DebugProbeError> {
        let capabilities = self.handle.read_capabilities()?;

        if capabilities.contains(jaylink::Capabilities::SELECT_IF) {
            if let Some(protocol) = protocol {
                let jlink_interface = match protocol {
                    WireProtocol::Swd => jaylink::Interface::Swd,
                    WireProtocol::Jtag => jaylink::Interface::Jtag,
                };

                if self
                    .handle
                    .read_available_interfaces()?
                    .any(|interface| interface == jlink_interface)
                {
                    // We can select the desired interface
                    self.handle.select_interface(jlink_interface)?;
                    Ok(protocol)
                } else {
                    Err(DebugProbeError::UnsupportedProtocol(protocol))
                }
            } else {
                // No special protocol request
                let current_protocol = self.handle.read_current_interface()?;

                match current_protocol {
                    jaylink::Interface::Swd => Ok(WireProtocol::Swd),
                    jaylink::Interface::Jtag => Ok(WireProtocol::Jtag),
                    x => unimplemented!("J-Link: Protocol {} is not yet supported.", x),
                }
            }
        } else {
            // Assume JTAG protocol if the probe does not support switching interfaces
            match protocol {
                Some(WireProtocol::Jtag) => Ok(WireProtocol::Jtag),
                Some(p) => Err(DebugProbeError::UnsupportedProtocol(p)),
                None => Ok(WireProtocol::Jtag),
            }
        }
    }

    fn read_dr(&mut self, register_bits: usize) -> Result<Vec<u8>, DebugProbeError> {
        log::debug!("Read {} bits from DR", register_bits);

        let tms_enter_shift = [true, false, false];

        // Last bit of data is shifted out when we exi the SHIFT-DR State
        let tms_shift_out_value = iter::repeat(false).take(register_bits - 1);

        let tms_enter_idle = [true, true, false];

        let mut tms = Vec::with_capacity(register_bits + 7);

        tms.extend_from_slice(&tms_enter_shift);
        tms.extend(tms_shift_out_value);
        tms.extend_from_slice(&tms_enter_idle);

        let tdi = iter::repeat(false).take(tms.len() + self.idle_cycles() as usize);

        // We have to stay in the idle cycle a bit
        tms.extend(iter::repeat(false).take(self.idle_cycles() as usize));

        let mut response = self.handle.jtag_io(tms, tdi)?;

        log::trace!("Response: {:?}", response);

        let _remainder = response.split_off(tms_enter_shift.len());

        let mut remaining_bits = register_bits;

        let mut result = Vec::new();

        while remaining_bits >= 8 {
            let byte = bits_to_byte(response.split_off(8)) as u8;
            result.push(byte);
            remaining_bits -= 8;
        }

        // Handle leftover bytes
        if remaining_bits > 0 {
            result.push(bits_to_byte(response.split_off(remaining_bits)) as u8);
        }

        log::debug!("Read from DR: {:?}", result);

        Ok(result)
    }

    /// Write IR register with the specified data. The
    /// IR register might have an odd length, so the dta
    /// will be truncated to `len` bits. If data has less
    /// than `len` bits, an error will be returned.
    fn write_ir(&mut self, data: &[u8], len: usize) -> Result<(), DebugProbeError> {
        log::debug!("Write IR: {:?}, len={}", data, len);

        // Check the bit length, enough data has to be
        // available
        if data.len() * 8 < len {
            todo!("Proper error for incorrect length");
        }

        // At least one bit has to be sent
        if len < 1 {
            todo!("Proper error for incorrect length");
        }

        let tms_enter_ir_shift = [true, true, false, false];

        // The last bit will be transmitted when exiting the shift state,
        // so we need to stay in the shift stay for one period less than
        // we have bits to transmit
        let tms_data = iter::repeat(false).take(len - 1);

        let tms_enter_idle = [true, true, false];

        let mut tms = Vec::with_capacity(tms_enter_ir_shift.len() + len + tms_enter_ir_shift.len());

        tms.extend_from_slice(&tms_enter_ir_shift);
        tms.extend(tms_data);
        tms.extend_from_slice(&tms_enter_idle);

        let tdi_enter_ir_shift = [false, false, false, false];

        // This is one less than the enter idle for tms, because
        // the last bit is transmitted when exiting the IR shift state
        let tdi_enter_idle = [false, false];

        let mut tdi = Vec::with_capacity(tdi_enter_ir_shift.len() + tdi_enter_idle.len() + len);

        tdi.extend_from_slice(&tdi_enter_ir_shift);

        let num_bytes = len / 8;

        let num_bits = len - (num_bytes * 8);

        for bytes in &data[..num_bytes] {
            let mut byte = *bytes;

            for _ in 0..8 {
                tdi.push(byte & 1 == 1);

                byte >>= 1;
            }
        }

        if num_bits > 0 {
            let mut remaining_byte = data[num_bytes];

            for _ in 0..num_bits {
                tdi.push(remaining_byte & 1 == 1);
                remaining_byte >>= 1;
            }
        }

        tdi.extend_from_slice(&tdi_enter_idle);

        log::trace!("tms: {:?}", tms);
        log::trace!("tdi: {:?}", tdi);

        let response = self.handle.jtag_io(tms, tdi)?;

        log::trace!("Response: {:?}", response);

        if len >= 8 {
            return Err(DebugProbeError::NotImplemented(
                "Not yet implemented for IR registers larger than 8 bit",
            ));
        }

        self.current_ir_reg = data[0] as u32;

        // Maybe we could return the previous state of the IR register here...

        Ok(())
    }

    fn write_dr(&mut self, data: &[u8], register_bits: usize) -> Result<Vec<u8>, DebugProbeError> {
        log::debug!("Write DR: {:?}, len={}", data, register_bits);

        let tms_enter_shift = [true, false, false];

        // Last bit of data is shifted out when we exi the SHIFT-DR State
        let tms_shift_out_value = iter::repeat(false).take(register_bits - 1);

        let tms_enter_idle = [true, true, false];

        let mut tms = Vec::with_capacity(register_bits + 7);

        tms.extend_from_slice(&tms_enter_shift);
        tms.extend(tms_shift_out_value);
        tms.extend_from_slice(&tms_enter_idle);

        let tdi_enter_shift = [false, false, false];

        let tdi_enter_idle = [false, false];

        // TODO: TDI data
        let mut tdi =
            Vec::with_capacity(tdi_enter_shift.len() + tdi_enter_idle.len() + register_bits);

        tdi.extend_from_slice(&tdi_enter_shift);

        let num_bytes = register_bits / 8;

        let num_bits = register_bits - (num_bytes * 8);

        for bytes in &data[..num_bytes] {
            let mut byte = *bytes;

            for _ in 0..8 {
                tdi.push(byte & 1 == 1);

                byte >>= 1;
            }
        }

        if num_bits > 0 {
            let mut remaining_byte = data[num_bytes];

            for _ in 0..num_bits {
                tdi.push(remaining_byte & 1 == 1);
                remaining_byte >>= 1;
            }
        }

        tdi.extend_from_slice(&tdi_enter_idle);

        // We need to stay in the idle cycle a bit
        tms.extend(iter::repeat(false).take(self.idle_cycles() as usize));
        tdi.extend(iter::repeat(false).take(self.idle_cycles() as usize));

        let mut response = self.handle.jtag_io(tms, tdi)?;

        log::trace!("Response: {:?}", response);

        let _remainder = response.split_off(tms_enter_shift.len());

        let mut remaining_bits = register_bits;

        let mut result = Vec::new();

        while remaining_bits >= 8 {
            let byte = bits_to_byte(response.split_off(8)) as u8;
            result.push(byte);
            remaining_bits -= 8;
        }

        // Handle leftover bytes
        if remaining_bits > 0 {
            result.push(bits_to_byte(response.split_off(remaining_bits)) as u8);
        }

        log::trace!("result: {:?}", result);

        Ok(result)
    }

    /// Try to perform a SWD line reset, followed by a read of the DPIDR register.
    ///
    /// Returns Ok if the read of the DPIDR register was succesful, and Err
    /// otherwise. In case of JLink Errors, the actual error is returned.
    ///
    /// If the first line reset fails, it is tried once again, as the target
    /// might be in the middle of a transfer the first time we try the reset.
    ///
    /// See section B4.3.3 in the ADIv5 Specification.
    fn swd_line_reset(&mut self) -> Result<(), DebugProbeError> {
        log::debug!("Performing line reset!");

        let mut swd_io = vec![true; 50];
        let mut direction = vec![true; 50];

        let (register_io, register_direction) =
            build_swd_transfer(PortType::DebugPort, TransferType::Read, 0);

        swd_io.extend_from_slice(&register_io);
        direction.extend_from_slice(&register_direction);

        let mut result = Ok(());

        for _ in 0..2 {
            let mut result_sequence = self.handle.swd_io(direction.clone(), swd_io.clone())?;

            // Ignore reset bits, idle bits, and request
            result_sequence.split_off(50 + 2 + 8);

            let ack = result_sequence.split_off(3).collect::<Vec<_>>();

            log::debug!("line reset ack: {:?}", ack);

            match &ack[..] {
                // OK response
                [true, false, false] => {
                    return Ok(());
                }
                // WAIT response
                [false, true, false] => {
                    result = Err(DapError::WaitResponse.into());
                }
                // FAULT response
                [false, false, true] => {
                    result = Err(DapError::FaultResponse.into());
                }
                _ => {
                    result = Err(DapError::NoAcknowledge.into());
                }
            }
        }

        // No acknowledge from the target, even if after line reset
        result
    }
}

impl DebugProbe for JLink {
    fn new_from_selector(
        selector: impl Into<DebugProbeSelector>,
    ) -> Result<Box<Self>, DebugProbeError> {
        let selector = selector.into();
        let mut jlinks = jaylink::scan_usb()?
            .filter_map(|usb_info| {
                if usb_info.vid() == selector.vendor_id && usb_info.pid() == selector.product_id {
                    let device = usb_info.open();
                    if let Some(serial_number) = selector.serial_number.as_deref() {
                        if device
                            .as_ref()
                            .map(|d| d.serial_string() == serial_number)
                            .unwrap_or(false)
                        {
                            Some(device)
                        } else {
                            None
                        }
                    } else {
                        Some(device)
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if jlinks.is_empty() {
            return Err(DebugProbeError::ProbeCouldNotBeCreated(
                super::ProbeCreationError::NotFound,
            ));
        } else if jlinks.len() > 1 {
            log::warn!("More than one matching J-Link was found. Opening the first one.")
        }
        let jlink_handle = jlinks.pop().unwrap()?;

        // Check which protocols are supported by the J-Link.
        //
        // If the J-Link has the SELECT_IF capability, we can just ask
        // it which interfaces it supports. If it doesn't have the capabilty,
        // we assume that it justs support JTAG. In that case, we will also
        // not be able to change protocols.

        let supported_protocols: Vec<WireProtocol> = if jlink_handle
            .read_capabilities()?
            .contains(jaylink::Capabilities::SELECT_IF)
        {
            let interfaces = jlink_handle.read_available_interfaces()?;

            let protocols: Vec<_> = interfaces.map(WireProtocol::try_from).collect();

            protocols
                .iter()
                .filter(|p| p.is_err())
                .for_each(|protocol| {
                    if let Err(JlinkError::UnknownInterface(interface)) = protocol {
                        log::warn!(
                            "J-Link returned interface {:?}, which is not supported by probe-rs.",
                            interface
                        );
                    }
                });

            // We ignore unknown protocols, the chance that this happens is pretty low,
            // and we can just work with the ones we know and support.
            protocols.into_iter().filter_map(Result::ok).collect()
        } else {
            // The J-Link cannot report which interfaces it supports, and cannot
            // switch interfaces. We assume it just supports JTAG.
            vec![WireProtocol::Jtag]
        };

        Ok(Box::new(JLink {
            handle: jlink_handle,
            swo_config: None,
            supported_protocols,
            jtag_idle_cycles: 0,
            protocol: None,
            current_ir_reg: 1,
            speed_khz: 0,
        }))
    }

    fn select_protocol(&mut self, protocol: WireProtocol) -> Result<(), DebugProbeError> {
        // try to select the interface

        let actual_protocol = self.select_interface(Some(protocol))?;

        if actual_protocol == protocol {
            self.protocol = Some(protocol);
            Ok(())
        } else {
            self.protocol = Some(actual_protocol);
            Err(DebugProbeError::UnsupportedProtocol(protocol))
        }
    }

    fn get_name(&self) -> &'static str {
        "J-Link"
    }

    fn speed(&self) -> u32 {
        self.speed_khz
    }

    fn set_speed(&mut self, speed_khz: u32) -> Result<u32, DebugProbeError> {
        if speed_khz == 0 || speed_khz >= 0xffff {
            return Err(DebugProbeError::UnsupportedSpeed(speed_khz));
        }

        let actual_speed_khz;
        if let Ok(speeds) = self.handle.read_speeds() {
            log::debug!("Supported speeds: {:?}", speeds);

            let speed_hz = 1000 * speed_khz;
            let div = (speeds.base_freq() + speed_hz - 1) / speed_hz;
            log::debug!("Divider: {}", div);
            let div = std::cmp::max(div, speeds.min_div() as u32);

            actual_speed_khz = ((speeds.base_freq() / div) + 999) / 1000;
            if actual_speed_khz > speed_khz {
                return Err(DebugProbeError::UnsupportedSpeed(speed_khz));
            }
        } else {
            actual_speed_khz = speed_khz;
        }

        self.handle
            .set_speed(CommunicationSpeed::khz(actual_speed_khz as u16).unwrap())?;
        self.speed_khz = actual_speed_khz;

        Ok(actual_speed_khz)
    }

    fn attach(&mut self) -> Result<(), super::DebugProbeError> {
        log::debug!("Attaching to J-Link");

        let configured_protocol = match self.protocol {
            Some(protocol) => protocol,
            None => {
                if self.supported_protocols.contains(&WireProtocol::Swd) {
                    WireProtocol::Swd
                } else {
                    // At least one protocol is always supported
                    *self.supported_protocols.first().unwrap()
                }
            }
        };

        let actual_protocol = self.select_interface(Some(configured_protocol))?;

        if actual_protocol != configured_protocol {
            log::warn!("Protocol {} is configured, but not supported by the probe. Using protocol {} instead", configured_protocol, actual_protocol);
        }

        log::debug!("Attaching with protocol '{}'", actual_protocol);

        // Get reference to JayLink instance
        let capabilities = self.handle.read_capabilities()?;

        // Log some information about the probe
        let serial = self.handle.serial_string().trim_start_matches('0');
        log::info!("J-Link: S/N: {}", serial);
        log::debug!("J-Link: Capabilities: {:?}", capabilities);
        let fw_version = self
            .handle
            .read_firmware_version()
            .unwrap_or_else(|_| "?".into());
        log::info!("J-Link: Firmware version: {}", fw_version);
        match self.handle.read_hardware_version() {
            Ok(hw_version) => log::info!("J-Link: Hardware version: {}", hw_version),
            Err(_) => log::info!("J-Link: Hardware version: ?"),
        };

        // Verify target voltage (VTref pin, mV). If this is 0, the device is not powered.
        let target_voltage = self.handle.read_target_voltage()?;
        if target_voltage == 0 {
            log::warn!("J-Link: Target voltage (VTref) is 0 V. Is your target device powered?");
        } else {
            log::info!(
                "J-Link: Target voltage: {:2.2} V",
                target_voltage as f32 / 1000f32
            );
        }

        match actual_protocol {
            WireProtocol::Jtag => {
                // try some JTAG stuff

                log::debug!("Resetting JTAG chain using trst");
                self.handle.reset_trst()?;

                log::debug!("Resetting JTAG chain by setting tms high for 32 bits");

                // Reset JTAG chain (5 times TMS high), and enter idle state afterwards
                let tms = vec![true, true, true, true, true, false];
                let tdi = iter::repeat(false).take(6);

                let response: Vec<_> = self.handle.jtag_io(tms, tdi)?.collect();

                log::debug!("Response to reset: {:?}", response);

                // try to read the idcode
                let idcode_bytes = self.read_dr(32)?;
                let idcode = u32::from_le_bytes((&idcode_bytes[..]).try_into().unwrap());

                log::debug!("IDCODE: {:#010x}", idcode);
            }
            WireProtocol::Swd => {
                // The following sequence is the DebugPortSetup sequence from the ARM debug sequences

                // Construct the JTAG to SWD sequence.
                let jtag_to_swd_sequence = [
                    false, true, true, true, true, false, false, true, true, true, true, false,
                    false, true, true, true,
                ];

                // Construct the entire init sequence.
                let swd_io_sequence =
                    // Send the reset sequence (> 50 0-bits).
                    iter::repeat(true).take(64)
                    // Send the JTAG to SWD sequence.
                    .chain(jtag_to_swd_sequence.iter().copied());

                // Construct the direction sequence for reset sequence.
                let direction =
                    // Send the reset sequence (> 50 0-bits).
                    iter::repeat(true).take(64)
                    // Send the JTAG to SWD sequence.
                    .chain(iter::repeat(true).take(16));

                // Send the init sequence.
                // We don't actually care about the response here.
                // A read on the DPIDR will finalize the init procedure and tell us if it worked.
                self.handle.swd_io(direction, swd_io_sequence)?;

                // Perform a line reset
                self.swd_line_reset()?;
                log::debug!("Sucessfully switched to SWD");

                // We are ready to debug.
            }
        }

        log::debug!("Attached succesfully");

        Ok(())
    }

    fn detach(&mut self) -> Result<(), super::DebugProbeError> {
        unimplemented!()
    }

    fn target_reset(&mut self) -> Result<(), super::DebugProbeError> {
        Err(super::DebugProbeError::NotImplemented("target_reset"))
    }

    fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
        self.handle.set_reset(false)?;
        Ok(())
    }

    fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
        self.handle.set_reset(true)?;
        Ok(())
    }

    fn get_riscv_interface(
        self: Box<Self>,
    ) -> Result<Option<RiscvCommunicationInterface>, DebugProbeError> {
        if self.supported_protocols.contains(&WireProtocol::Jtag) {
            Ok(Some(RiscvCommunicationInterface::new(self)?))
        } else {
            Ok(None)
        }
    }

    fn get_swo_interface(&self) -> Option<&dyn SwoAccess> {
        Some(self as _)
    }

    fn get_swo_interface_mut(&mut self) -> Option<&mut dyn SwoAccess> {
        Some(self as _)
    }

    fn get_arm_interface<'probe>(
        self: Box<Self>,
    ) -> Result<Option<Box<dyn ArmProbeInterface + 'probe>>, DebugProbeError> {
        if self.supported_protocols.contains(&WireProtocol::Swd) {
            let interface = ArmCommunicationInterface::new(self, true)?;

            Ok(Some(Box::new(interface)))
        } else {
            Ok(None)
        }
    }

    fn has_arm_interface(&self) -> bool {
        self.supported_protocols.contains(&WireProtocol::Swd)
    }

    fn has_riscv_interface(&self) -> bool {
        self.supported_protocols.contains(&WireProtocol::Jtag)
    }
}

impl JTAGAccess for JLink {
    /// Read the data register
    fn read_register(&mut self, address: u32, len: u32) -> Result<Vec<u8>, DebugProbeError> {
        let address_bits = address.to_le_bytes();

        // TODO: This is limited to 5 bit addresses for now
        if address > 0x1f {
            return Err(DebugProbeError::NotImplemented(
                "JTAG Register addresses are fixed to 5 bits",
            ));
        }

        if self.current_ir_reg != address {
            // Write IR register
            self.write_ir(&address_bits[..1], 5)?;
        }

        // read DR register
        self.read_dr(len as usize)
    }

    /// Write the data register
    fn write_register(
        &mut self,
        address: u32,
        data: &[u8],
        len: u32,
    ) -> Result<Vec<u8>, DebugProbeError> {
        let address_bits = address.to_le_bytes();

        // TODO: This is limited to 5 bit addresses for now
        if address > 0x1f {
            return Err(DebugProbeError::NotImplemented(
                "JTAG Register addresses are fixed to 5 bits",
            ));
        }

        if self.current_ir_reg != address {
            // Write IR register
            self.write_ir(&address_bits[..1], 5)?;
        }

        // write DR register
        self.write_dr(data, len as usize)
    }

    fn set_idle_cycles(&mut self, idle_cycles: u8) {
        self.jtag_idle_cycles = idle_cycles;
    }

    fn into_probe(self: Box<Self>) -> Box<dyn DebugProbe> {
        self
    }
}

impl<'a> AsRef<dyn DebugProbe + 'a> for JLink {
    fn as_ref(&self) -> &(dyn DebugProbe + 'a) {
        self
    }
}

impl<'a> AsMut<dyn DebugProbe + 'a> for JLink {
    fn as_mut(&mut self) -> &mut (dyn DebugProbe + 'a) {
        self
    }
}

#[derive(Debug, PartialEq)]
enum TransferType {
    Read,
    Write(u32),
}

fn build_swd_transfer(
    port: PortType,
    direction: TransferType,
    address: u16,
) -> (Vec<bool>, Vec<bool>) {
    // JLink operates on raw SWD bit sequences.
    // So we need to manually assemble the read and write bitsequences.
    // The following code with the comments hopefully explains well enough how it works.
    // `true` means `1` and `false` means `0` for the SWDIO sequence.
    // `true` means `drive line` and `false` means `open drain` for the direction sequence.

    // First we determine the APnDP bit.
    let port = match port {
        PortType::DebugPort => false,
        PortType::AccessPort(_) => true,
    };

    // Set direction bit to 1 for reads.
    let direction_bit = direction == TransferType::Read;

    // Then we determine the address bits.
    // Only bits 2 and 3 are relevant as we use byte addressing but can only read 32bits
    // which means we can skip bits 0 and 1. The ADI specification is defined like this.
    let a2 = (address >> 2) & 0x01 == 1;
    let a3 = (address >> 3) & 0x01 == 1;

    // Now we assemble an SWD request.
    let mut swd_io_sequence = vec![
        // First we make sure we have the SDWIO line on idle for at least 2 clock cylces.
        false, // Line idle.
        false, // Line idle.
        // Then we assemble the actual request.
        true,                           // Start bit (always 1).
        port,                           // APnDP (0 for DP, 1 for AP).
        direction_bit,                  // RnW (0 for Write, 1 for Read).
        a2,                             // Address bit 2.
        a3,                             // Address bit 3,
        port ^ direction_bit ^ a2 ^ a3, // Odd parity bit over APnDP, RnW a2 and a3
        false,                          // Stop bit (always 0).
        true,                           // Park bit (always 1).
        // Theoretically the spec says that there is a turnaround bit required here, where no clock is driven.
        // This seems to not be the case in actual implementations. So we do not insert this bit either!
        // false,                 // Turnaround bit.
        false, // ACK bit.
        false, // ACK bit.
        false, // ACK bit.
    ];

    if let TransferType::Write(mut value) = direction {
        // For writes, we need to add two turnaround bits.
        // Theoretically the spec says that there is only one turnaround bit required here, where no clock is driven.
        // This seems to not be the case in actual implementations. So we insert two turnaround bits here!
        swd_io_sequence.extend_from_slice(&[false, false]);

        // Now we add all the data bits to the sequence and in the same loop we also calculate the parity bit.
        let mut parity = false;
        for _ in 0..32 {
            let bit = value & 1 == 1;
            swd_io_sequence.push(bit);
            parity ^= bit;
            value >>= 1;
        }

        swd_io_sequence.push(parity);
    } else {
        // Handle Read
        // Add the data bits to the SWDIO sequence.
        swd_io_sequence.extend_from_slice(&[false; 32]);

        // Add the parity bit to the sequence.
        swd_io_sequence.push(false);

        // Finally add the turnaround bit to the sequence.
        swd_io_sequence.push(false);
    }

    // Assemble the direction sequence.
    let direction_sequence = iter::repeat(true)
        .take(2) // Transmit 2 Line idle bits.
        .chain(iter::repeat(true).take(8)) // Transmit 8 Request bits
        // Here *should* be a Trn bit, but since something with the spec is akward we leave it away.
        // See comments above!
        .chain(iter::repeat(false).take(3)); // Receive 3 Ack bits.

    let direction_sequence: Vec<bool> = if direction == TransferType::Read {
        direction_sequence
            .chain(iter::repeat(false).take(32)) // Receive 32 Data bits.
            .chain(iter::repeat(false).take(1)) // Receive 1 Parity bit.
            .chain(iter::repeat(false).take(1)) // Transmit turnaround bit
            .collect()
    } else {
        direction_sequence
            .chain(iter::repeat(false).take(2)) // Transmit two turnaround bits.
            .chain(iter::repeat(true).take(32)) // Transmit 32 Data bits.
            .chain(iter::repeat(true).take(1)) // Transmit 1 Parity bit.
            .collect()
    };

    assert_eq!(
        swd_io_sequence.len(),
        direction_sequence.len(),
        "IO and direction sequences need to have the same length."
    );

    (swd_io_sequence, direction_sequence)
}

impl DAPAccess for JLink {
    fn read_register(&mut self, port: PortType, address: u16) -> Result<u32, DebugProbeError> {
        // JLink operates on raw SWD bit sequences.
        // So we need to manually assemble the read and write bitsequences.
        // The following code with the comments hopefully explains well enough how it works.
        // `true` means `1` and `false` means `0` for the SWDIO sequence.
        // `true` means `drive line` and `false` means `open drain` for the direction sequence.

        let (swd_io_sequence, direction) = build_swd_transfer(port, TransferType::Read, address);

        // Now we try to issue the request until it fails or succeeds.
        // If we timeout we retry a maximum of 5 times.
        for retry in 0..5 {
            // Transmit the sequence and record the line sequence for the ack bits.
            let mut result_sequence = self
                .handle
                .swd_io(direction.clone(), swd_io_sequence.iter().copied())?;

            // Throw away the two idle bits.
            result_sequence.split_off(2);
            // Throw away the request bits.
            result_sequence.split_off(8);

            // Get the ack.
            let ack = result_sequence.split_off(3).collect::<Vec<_>>();

            // When all bits are high, this means we didn't get any response from the
            // target, which indicates a protocol error.
            if ack[0] && ack[1] && ack[2] {
                log::debug!("DAP NACK");

                // Because we clock the SWDCLK line after receving the WAIT response,
                // the target might be in weird state. If we perform a line reset,
                // we should be able to recover from this.
                self.swd_line_reset()?;

                // Retry operation again
                continue;
            }
            if ack[1] {
                // If ack[1] is set the host must retry the request. So let's do that right away!
                log::debug!("DAP WAIT, retries remaining {}.", 5 - retry);

                // Because we use overrun detection, we now have to clear the overrun error
                let mut abort = Abort(0);

                abort.set_orunerrclr(true);

                DAPAccess::write_register(
                    self,
                    PortType::DebugPort,
                    Abort::ADDRESS as u16,
                    abort.into(),
                )?;

                continue;
            }
            if ack[2] {
                log::debug!("DAP FAULT");

                // A fault happened during operation.

                // To get a clue about the actual fault we read the ctrl register,
                // which will have the fault status flags set.
                let response =
                    DAPAccess::read_register(self, PortType::DebugPort, Ctrl::ADDRESS as u16)?;
                let ctrl = Ctrl::from(response);
                log::debug!(
                    "Writing DAP register failed. Ctrl/Stat register value is: {:#?}",
                    ctrl
                );

                // Check the reason for the fault
                // Other fault reasons than overrun or write error are not handled yet.
                if ctrl.sticky_orun() || ctrl.sticky_err() {
                    // We did not handle a WAIT state properly

                    // Because we use overrun detection, we now have to clear the overrun error
                    let mut abort = Abort(0);

                    // Clear sticky error flags
                    abort.set_orunerrclr(ctrl.sticky_orun());
                    abort.set_stkerrclr(ctrl.sticky_err());

                    DAPAccess::write_register(
                        self,
                        PortType::DebugPort,
                        Abort::ADDRESS as u16,
                        abort.into(),
                    )?;
                    continue;
                }

                return Err(DapError::FaultResponse.into());
            }

            // If we are reading an AP register we only get the actual result in the next transaction.
            // So we issue a special transaction to get the read value.
            if let PortType::AccessPort(_) = port {
                // We read the RDBUFF register to get the value of the last AP transaction.
                // This special register just returns the last read value with no side-effects like auto-increment.
                return DAPAccess::read_register(self, PortType::DebugPort, RdBuff::ADDRESS as u16);
            } else {
                // Take the data bits and convert them into a 32bit int.
                let register_val = result_sequence.split_off(32);
                let value = bits_to_byte(register_val);

                // Make sure the parity is correct.
                return result_sequence
                    .next()
                    .and_then(|parity| {
                        if (value.count_ones() % 2 == 1) == parity {
                            log::trace!("DAP read {}.", value);
                            Some(value)
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| {
                        log::error!("DAP read fault.");
                        DapError::IncorrectParity.into()
                    });

                // Don't care about the Trn bit at the end.
            }
        }

        // If we land here, the DAP operation timed out.
        log::error!("DAP read timeout.");
        Err(DebugProbeError::Timeout)
    }

    fn write_register(
        &mut self,
        port: PortType,
        address: u16,
        value: u32,
    ) -> Result<(), DebugProbeError> {
        // JLink operates on raw SWD bit sequences.
        // So we need to manually assemble the read and write bitsequences.
        // The following code with the comments hopefully explains well enough how it works.
        // `true` means `1` and `false` means `0` for the SWDIO sequence.
        // `true` means `drive line` and `false` means `open drain` for the direction sequence.

        let (mut swd_io_sequence, mut direction) =
            build_swd_transfer(port, TransferType::Write(value), address);

        // Add 8 idle cycles to ensure the write is performed.
        // See section B4.1.1 in the ARM Debug Interface specification.
        //
        // This doesn't have to be done if the write is directly followed by another request,
        // but until batching is implemented, this is the safest way.
        for _ in 0..8 {
            swd_io_sequence.push(false);
            direction.push(true);
        }

        // Now we try to issue the request until it fails or succeeds.
        // If we timeout we retry a maximum of 5 times.
        for retry in 0..5 {
            // Transmit the sequence and record the line sequence for the ack and data bits.
            let mut result_sequence = self
                .handle
                .swd_io(direction.clone(), swd_io_sequence.iter().copied())?;

            // Throw away the two idle bits.
            result_sequence.split_off(2);
            // Throw away the request bits.
            result_sequence.split_off(8);

            // Get the ack.
            let ack = result_sequence.by_ref().take(3).collect::<Vec<_>>();

            // When all bits are high, this means we didn't get any response from the
            // target, which indicates a protocol error.
            if ack[0] && ack[1] && ack[2] {
                log::debug!("DAP NACK");

                // Because we clock the SWDCLK line after receving the WAIT response,
                // the target might be in weird state. If we perform a line reset,
                // we should be able to recover from this.
                self.swd_line_reset()?;

                // Retry operation
                continue;
            }

            if ack[1] {
                // If ack[1] is set the host must retry the request. So let's do that right away!
                log::debug!("DAP WAIT, retries remaining {}.", 5 - retry);

                let mut abort = Abort(0);

                abort.set_orunerrclr(true);

                // Because we use overrun detection, we now have to clear the overrun error
                DAPAccess::write_register(
                    self,
                    PortType::DebugPort,
                    Abort::ADDRESS as u16,
                    abort.into(),
                )?;

                continue;
            }

            if ack[2] {
                log::debug!("DAP FAULT");
                // A fault happened during operation.

                // To get a clue about the actual fault we read the ctrl register,
                // which will have the fault status flags set.

                let response =
                    DAPAccess::read_register(self, PortType::DebugPort, Ctrl::ADDRESS as u16)?;

                let ctrl = Ctrl::from(response);
                log::trace!(
                    "Writing DAP register failed. Ctrl/Stat register value is: {:#?}",
                    ctrl
                );

                // Check the reason for the fault
                // Other fault reasons than overrun or write error are not handled yet.
                if ctrl.sticky_orun() || ctrl.sticky_err() {
                    // We did not handle a WAIT state properly

                    // Because we use overrun detection, we now have to clear the overrun error
                    let mut abort = Abort(0);

                    // Clear sticky error flags
                    abort.set_orunerrclr(ctrl.sticky_orun());
                    abort.set_stkerrclr(ctrl.sticky_err());

                    DAPAccess::write_register(
                        self,
                        PortType::DebugPort,
                        Abort::ADDRESS as u16,
                        abort.into(),
                    )?;
                    continue;
                }

                return Err(DapError::FaultResponse.into());
            }

            // Since this is a write request, we don't care about the part after the ack bits.
            // So we just discard the Trn + Data + Parity bits.
            log::trace!("DAP wrote {}.", value);
            return Ok(());
        }

        // If we land here, the DAP operation timed out.
        log::error!("DAP write timeout.");
        Err(DebugProbeError::Timeout)
    }

    fn into_probe(self: Box<Self>) -> Box<dyn DebugProbe> {
        self
    }

    fn swj_sequence(&mut self, bit_len: u8, bits: u64) -> Result<(), DebugProbeError> {
        let mut bit_vector = Vec::with_capacity(bit_len as usize);

        let mut bits = bits;

        for _ in 0..bit_len {
            bit_vector.push(bits & 1 == 1);

            bits >>= 1;
        }

        let direction_bits = iter::repeat(true).take(bit_len as usize);

        self.handle.swd_io(direction_bits, bit_vector.into_iter())?;

        Ok(())
    }

    fn swj_pins(
        &mut self,
        pin_out: u32,
        pin_select: u32,
        _pin_wait: u32,
    ) -> Result<u32, DebugProbeError> {
        /*
        Bit 0: SWCLK/TCK
        Bit 1: SWDIO/TMS
        Bit 2: TDI
        Bit 3: TDO
        Bit 5: nTRST
        Bit 7: nRESET
        */

        if pin_select & (1 << 0) != 0 {
            // set SWCLK/TCK

            // This seems not possible on JLINK

            log::warn!("Unable to control SWCLK/TCK pin on JLink");
        }

        if pin_select & (1 << 1) != 0 {
            // set SWDIO/TMS

            self.handle.set_tms(pin_out & (1 << 1) != 0)?;
        }

        if pin_select & (1 << 2) != 0 {
            // set TDI

            self.handle.set_tdi(pin_out & (1 << 2) != 0)?;
        }

        if pin_select & (1 << 3) != 0 {
            // set TDO

            // This seems not possible on JLINK

            log::warn!("Unable to control TDO pin on JLink");
        }

        if pin_select & (1 << 5) != 0 {
            // set nTRST

            self.handle.set_trst(pin_out & (1 << 5) != 0)?;
        }

        if pin_select & (1 << 7) != 0 {
            // set nRESET

            self.handle.set_reset(pin_out & (1 << 7) != 0)?;
        }

        // We cannot read back the value of these pins
        Ok(0xffff_ffff)
    }
}

impl SwoAccess for JLink {
    fn enable_swo(&mut self, config: &SwoConfig) -> Result<(), ProbeRsError> {
        self.swo_config = Some(*config);
        self.handle
            .swo_start_uart(config.baud(), SWO_BUFFER_SIZE.into())
            .map_err(|e| ProbeRsError::Probe(DebugProbeError::ArchitectureSpecific(Box::new(e))))?;
        Ok(())
    }

    fn disable_swo(&mut self) -> Result<(), ProbeRsError> {
        self.swo_config = None;
        self.handle
            .swo_stop()
            .map_err(|e| ProbeRsError::Probe(DebugProbeError::ArchitectureSpecific(Box::new(e))))?;
        Ok(())
    }

    fn swo_buffer_size(&mut self) -> Option<usize> {
        Some(SWO_BUFFER_SIZE.into())
    }

    fn read_swo_timeout(&mut self, timeout: std::time::Duration) -> Result<Vec<u8>, ProbeRsError> {
        let end = std::time::Instant::now() + timeout;
        let mut buf = vec![0; SWO_BUFFER_SIZE.into()];

        let poll_interval = self
            .swo_poll_interval_hint(&self.swo_config.unwrap())
            .unwrap();

        let mut bytes = vec![];
        loop {
            let data = self.handle.swo_read(&mut buf).map_err(|e| {
                ProbeRsError::Probe(DebugProbeError::ArchitectureSpecific(Box::new(e)))
            })?;
            bytes.extend(data.as_ref());
            let now = std::time::Instant::now();
            if now + poll_interval < end {
                std::thread::sleep(poll_interval);
            } else {
                break;
            }
        }
        Ok(bytes)
    }
}

fn bits_to_byte(bits: impl IntoIterator<Item = bool>) -> u32 {
    let mut bit_val = 0u32;

    for (index, bit) in bits.into_iter().take(32).enumerate() {
        if bit {
            bit_val |= 1 << index;
        }
    }

    bit_val
}

pub(crate) fn list_jlink_devices() -> Vec<DebugProbeInfo> {
    match jaylink::scan_usb() {
        Ok(devices) => devices
            .map(|device_info| {
                let vid = device_info.vid();
                let pid = device_info.pid();
                let (serial, product) = if let Ok(device) = device_info.open() {
                    let serial = device.serial_string();
                    let serial = if serial.is_empty() {
                        None
                    } else {
                        Some(serial.to_owned())
                    };
                    let product = device.product_string();
                    let product = if product.is_empty() {
                        None
                    } else {
                        Some(product.to_owned())
                    };
                    (serial, product)
                } else {
                    (None, None)
                };
                DebugProbeInfo::new(
                    format!(
                        "J-Link{}",
                        product.map(|p| format!(" ({})", p)).unwrap_or_default()
                    ),
                    vid,
                    pid,
                    serial,
                    DebugProbeType::JLink,
                )
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

impl From<jaylink::Error> for DebugProbeError {
    fn from(e: jaylink::Error) -> DebugProbeError {
        DebugProbeError::ProbeSpecific(Box::new(e))
    }
}

#[derive(Debug, Error)]
pub enum JlinkError {
    #[error("Unknown interface reported by J-Link: {0:?}")]
    UnknownInterface(jaylink::Interface),
}

impl TryFrom<jaylink::Interface> for WireProtocol {
    type Error = JlinkError;

    fn try_from(interface: Interface) -> Result<Self, Self::Error> {
        match interface {
            Interface::Jtag => Ok(WireProtocol::Jtag),
            Interface::Swd => Ok(WireProtocol::Swd),
            unknown_interface => Err(JlinkError::UnknownInterface(unknown_interface)),
        }
    }
}
