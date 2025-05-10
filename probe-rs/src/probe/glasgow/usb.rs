use std::io::{Read, Write};

use nusb::{
    Interface, MaybeFuture,
    transfer::{Bulk, Direction, In, Out},
};

use crate::probe::{
    DebugProbeError, DebugProbeSelector, ProbeCreationError,
    glasgow::mux::{DiscoveryError, hexdump},
};

pub(super) const VID_QIHW: u16 = 0x20b7;
pub(super) const PID_GLASGOW: u16 = 0x9db1;

pub struct GlasgowUsbDevice {
    out_iface: Interface,
    in_iface: Interface,
    out_ep_num: u8,
    in_ep_num: u8,
}

impl GlasgowUsbDevice {
    pub fn new_from_selector(selector: &DebugProbeSelector) -> Result<Self, ProbeCreationError> {
        if selector.vendor_id != VID_QIHW && selector.product_id != PID_GLASGOW {
            Err(ProbeCreationError::NotFound)?
        }
        let Some(serial) = selector.serial_number.as_ref() else {
            Err(ProbeCreationError::NotFound)?
        };
        let parts = serial.split(":").collect::<Vec<_>>();
        let [serial, in_iface_num, out_iface_num] = parts[..] else {
            Err(DiscoveryError::InvalidFormat)?
        };
        let in_iface_num: u8 = in_iface_num
            .parse()
            .map_err(|_| DiscoveryError::InvalidFormat)?;
        let out_iface_num: u8 = out_iface_num
            .parse()
            .map_err(|_| DiscoveryError::InvalidFormat)?;

        let selector = DebugProbeSelector {
            serial_number: Some(serial.to_owned()),
            ..selector.clone()
        };
        let device_info = nusb::list_devices()
            .wait()?
            .find(|device| selector.matches(device))
            .ok_or(ProbeCreationError::NotFound)?;
        let device = device_info.open().wait()?;

        let mut in_ep_num = None;
        let mut out_ep_num = None;
        if let Ok(config) = device.active_configuration() {
            if let Some(interface) = config.interfaces().nth(in_iface_num as usize) {
                if let Some(altsetting) = interface.alt_settings().nth(1) {
                    if let Some(endpoint) = altsetting.endpoints().next() {
                        if endpoint.direction() == Direction::In {
                            in_ep_num = Some(endpoint.address());
                        }
                    }
                }
            }
            if let Some(interface) = config.interfaces().nth(out_iface_num as usize) {
                if let Some(altsetting) = interface.alt_settings().nth(1) {
                    if let Some(endpoint) = altsetting.endpoints().next() {
                        if endpoint.direction() == Direction::Out {
                            out_ep_num = Some(endpoint.address());
                        }
                    }
                }
            }
        }

        let (Some(in_ep_num), Some(out_ep_num)) = (in_ep_num, out_ep_num) else {
            Err(DiscoveryError::InvalidInterfaces)?
        };
        tracing::info!(
            "opened Glasgow Interface Explorer (IN {in_iface_num}/{in_ep_num:#04x}, OUT {out_iface_num}/{out_ep_num:#04x})"
        );

        // This makes our endpoints available for use.
        let out_iface = device.claim_interface(out_iface_num).wait()?;
        let in_iface = device.claim_interface(in_iface_num).wait()?;

        // This takes the applet out of reset.
        out_iface.set_alt_setting(1).wait()?;
        in_iface.set_alt_setting(1).wait()?;

        Ok(Self {
            out_iface,
            in_iface,
            out_ep_num,
            in_ep_num,
        })
    }

    pub fn transfer(
        &mut self,
        output: Vec<u8>,
        mut input: impl FnMut(Vec<u8>) -> Result<bool, DebugProbeError>,
    ) -> Result<(), DebugProbeError> {
        if !output.is_empty() {
            tracing::trace!("OUT URB: {}", hexdump(&output));

            let tx = self
                .out_iface
                .endpoint::<Bulk, Out>(self.out_ep_num)
                .map_err(std::io::Error::from)
                .map_err(DebugProbeError::Usb)?;

            let mut writer = tx.writer(output.len());

            writer.write_all(&output).map_err(DebugProbeError::Usb)?;
            writer.flush_end().map_err(DebugProbeError::Usb)?;
        }

        let rx = self
            .in_iface
            .endpoint::<Bulk, In>(self.in_ep_num)
            .map_err(std::io::Error::from)
            .map_err(DebugProbeError::Usb)?;

        let mut reader = rx.reader(65536);

        let mut buffer = Vec::new();

        while !input(buffer)? {
            buffer = vec![0u8; 65536];

            let buffer_len = reader.read(&mut buffer[..]).map_err(DebugProbeError::Usb)?;
            buffer.truncate(buffer_len);

            tracing::trace!("IN URB: {}", hexdump(&buffer));
        }
        Ok(())
    }
}
