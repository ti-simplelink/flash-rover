// Copyright (c) 2020 , Texas Instruments.
// Licensed under the BSD-3-Clause license
// (see LICENSE or <https://opensource.org/licenses/BSD-3-Clause>) All files in the project
// notice may not be copied, modified, or distributed except according to those terms.

use std::io::{self, Write};
use std::thread;
use std::time::{Duration, SystemTime};

use snafu::{Backtrace, ResultExt, Snafu};
use tempfile::TempPath;

use dss::com::ti::debug::engine::scripting::{Memory, Register};

use crate::assets;
use crate::types::{Device, SpiPin, SpiPins};
use crate::xflash::Xflash;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("A DSS error occured: {}", source))]
    DssError {
        source: dss::Error,
        backtrace: Backtrace,
    },
    #[snafu(display("No response received from firmware"))]
    NoResponse { backtrace: Backtrace },
    #[snafu(display("Invalid response received from firmware: {:?}", bytes))]
    InvalidResponse {
        bytes: [u32; 4],
        backtrace: Backtrace,
    },
    #[snafu(display("Bad response received from firmware: {:?}", response))]
    BadResponse {
        response: Response,
        backtrace: Backtrace,
    },
    #[snafu(display("An error response received from firmware with value: {}", kind))]
    ErrorResponse { kind: u32, backtrace: Backtrace },
    #[snafu(display("Tool timed out waiting for a response from firmware"))]
    FirmwareTimeout { backtrace: Backtrace },
    #[snafu(display("Unable to create the firmware binary asset: {}", source))]
    FirmwareAsset {
        source: io::Error,
        backtrace: Backtrace,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
enum Command {
    GetXflashInfo,
    SectorErase { offset: u32, length: u32 },
    MassErase,
    ReadBlock { offset: u32, length: u32 },
    WriteBlock { offset: u32, length: u32 },
}

impl Command {
    fn to_bytes(&self) -> [u32; 4] {
        use Command::*;

        match self {
            GetXflashInfo => [0xC0_u32.to_le(), 0, 0, 0],
            SectorErase { offset, length } => [0xC1_u32.to_le(), offset.to_le(), length.to_le(), 0],
            MassErase => [0xC2_u32.to_le(), 0, 0, 0],
            ReadBlock { offset, length } => [0xC3_u32.to_le(), offset.to_le(), length.to_le(), 0],
            WriteBlock { offset, length } => [0xC4_u32.to_le(), offset.to_le(), length.to_le(), 0],
        }
    }
}

#[derive(Debug)]
pub enum Response {
    Ok,
    XflashInfo(Xflash),
}

impl Response {
    fn from_bytes(bytes: &[u32; 4]) -> Result<Self> {
        const OK_VAL: u32 = 0xD0_u32.to_le();
        const XFLASHINFO_VAL: u32 = 0xD1_u32.to_le();

        let rsp = match bytes {
            [OK_VAL, 0, 0, 0] => Response::Ok,
            [XFLASHINFO_VAL, mid, did, 0] => Response::XflashInfo(Xflash::from_id(*mid, *did)),
            _ => InvalidResponse { bytes: *bytes }.fail()?,
        };
        Ok(rsp)
    }
}

const SRAM_START: u32 = 0x2000_0000;
const STACK_ADDR: u32 = SRAM_START;
const RESET_ISR: u32 = SRAM_START + 0x04;

const CONF_START: u32 = 0x2000_3000;

const CONF_VALID: u32 = CONF_START;
const CONF_SPI_MISO: u32 = CONF_START + 0x04;
const CONF_SPI_MOSI: u32 = CONF_START + 0x08;
const CONF_SPI_CLK: u32 = CONF_START + 0x0C;
const CONF_SPI_CSN: u32 = CONF_START + 0x10;

const DOORBELL_START: u32 = 0x2000_3100;

const DOORBELL_CMD_KIND: u32 = DOORBELL_START;
const DOORBELL_CMD_ARG0: u32 = DOORBELL_START + 0x04;
const DOORBELL_CMD_ARG1: u32 = DOORBELL_START + 0x08;
const DOORBELL_CMD_ARG2: u32 = DOORBELL_START + 0x0C;

const DOORBELL_RSP_KIND: u32 = DOORBELL_START + 0x10;
const DOORBELL_RSP_VAL0: u32 = DOORBELL_START + 0x14;
const DOORBELL_RSP_VAL1: u32 = DOORBELL_START + 0x18;
const DOORBELL_RSP_VAL2: u32 = DOORBELL_START + 0x1C;

const BUF_START: u32 = 0x2000_4000;
pub const BUF_SIZE: u32 = 0x1000;

pub struct Firmware<'a> {
    memory: Memory<'a>,
    binary: TempPath,
}

impl<'a> Firmware<'a> {
    pub fn new(memory: Memory<'a>, device: Device) -> Result<Firmware<'a>> {
        let binary = Firmware::create_firmware_binary(device)?;

        Ok(Self { memory, binary })
    }

    pub fn inject(&self, spi_pins: Option<SpiPins>) -> Result<()> {
        let binary_path = self.binary.to_string_lossy().to_owned();

        self.dss_load_raw(&binary_path)?;

        if let Some(spi_pins) = spi_pins {
            self.dss_write_data(CONF_VALID, 1)?;
            self.dss_write_data(CONF_SPI_MISO, spi_pins[SpiPin::Miso] as _)?;
            self.dss_write_data(CONF_SPI_MOSI, spi_pins[SpiPin::Mosi] as _)?;
            self.dss_write_data(CONF_SPI_CLK, spi_pins[SpiPin::Clk] as _)?;
            self.dss_write_data(CONF_SPI_CSN, spi_pins[SpiPin::Csn] as _)?;
        }

        let stack_addr = self.dss_read_data(STACK_ADDR)?;
        let reset_isr = self.dss_read_data(RESET_ISR)?;

        self.dss_write_register(Register::MSP, stack_addr)?;
        self.dss_write_register(Register::PC, reset_isr)?;
        self.dss_write_register(Register::LR, 0xFFFF_FFFF)?;

        Ok(())
    }

    pub fn get_xflash_info(&self) -> Result<Xflash> {
        let command = Command::GetXflashInfo;
        match self.send_command(command, None)? {
            Response::XflashInfo(xflash) => Ok(xflash),
            response => BadResponse { response }.fail(),
        }
    }

    pub fn sector_erase(&self, offset: u32, length: u32) -> Result<()> {
        // Plus one for margin, as the write range can touch two sectors: one at
        // the beginnning and one at the end
        let num_sectors = length / BUF_SIZE + 1;
        let timeout = num_sectors * Duration::from_millis(500);

        let command = Command::SectorErase { offset, length };
        match self.send_command(command, Some(timeout))? {
            Response::Ok => Ok(()),
            response => BadResponse { response }.fail(),
        }
    }

    pub fn mass_erase(&self) -> Result<()> {
        const TIMEOUT: Duration = Duration::from_secs(240);

        let command = Command::MassErase;
        match self.send_command(command, Some(TIMEOUT))? {
            Response::Ok => {}
            response => BadResponse { response }.fail()?,
        }

        Ok(())
    }

    pub fn read_data(&self, offset: u32, length: u32) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }

        let mut data = Vec::with_capacity(length as _);

        let mut offset = offset;
        let mut length = length;

        // let mut zero_vec = Vec::with_capacity(BUF_SIZE as _);
        // zero_vec.resize_with(BUF_SIZE as _, || 0);

        while length > 0 {
            let ilength = std::cmp::min(length, BUF_SIZE as _);

            // self.dss_write_datas(BUF_START, &zero_vec)?;

            let command = Command::ReadBlock {
                offset,
                length: ilength,
            };
            match self.send_command(command, None)? {
                Response::Ok => {}
                response => BadResponse { response }.fail()?,
            }

            let values = self.dss_read_datas(BUF_START, ilength)?;
            data.extend_from_slice(&values);

            offset += ilength;
            length -= ilength;
        }

        Ok(data)
    }

    pub fn write_data(&self, offset: u32, values: &[u8]) -> Result<()> {
        if values.is_empty() {
            return Ok(());
        }

        let mut offset = offset;

        for chunk in values.chunks(BUF_SIZE as _) {
            self.dss_write_datas(BUF_START, chunk)?;

            let command = Command::WriteBlock {
                offset,
                length: chunk.len() as _,
            };
            match self.send_command(command, None)? {
                Response::Ok => {}
                response => BadResponse { response }.fail()?,
            }

            offset += chunk.len() as u32;
        }

        Ok(())
    }

    fn send_command(&self, command: Command, timeout: Option<Duration>) -> Result<Response> {
        let bytes = command.to_bytes();

        self.dss_write_data(DOORBELL_CMD_ARG2, bytes[3])?;
        self.dss_write_data(DOORBELL_CMD_ARG1, bytes[2])?;
        self.dss_write_data(DOORBELL_CMD_ARG0, bytes[1])?;
        self.dss_write_data(DOORBELL_CMD_KIND, bytes[0])?;

        const DWELL_TIME: Duration = Duration::from_millis(100);
        const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

        let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);

        let sys_time = SystemTime::now();

        while self.dss_read_data(DOORBELL_CMD_KIND)? != 0
            && sys_time.elapsed().unwrap_or_default() < timeout
        {
            thread::sleep(DWELL_TIME);
        }

        if sys_time.elapsed().unwrap_or_default() >= timeout {
            return FirmwareTimeout {}.fail();
        }

        let sys_time = SystemTime::now();

        while self.dss_read_data(DOORBELL_RSP_KIND)? == 0
            && sys_time.elapsed().unwrap_or_default() < timeout
        {
            thread::sleep(DWELL_TIME);
        }

        if sys_time.elapsed().unwrap_or_default() >= timeout {
            return FirmwareTimeout {}.fail();
        }

        let bytes: [u32; 4] = [
            self.dss_read_data(DOORBELL_RSP_KIND)?,
            self.dss_read_data(DOORBELL_RSP_VAL0)?,
            self.dss_read_data(DOORBELL_RSP_VAL1)?,
            self.dss_read_data(DOORBELL_RSP_VAL2)?,
        ];

        self.dss_write_data(DOORBELL_RSP_KIND, 0)?;

        Ok(Response::from_bytes(&bytes)?)
    }

    fn dss_write_data(&self, address: u32, value: u32) -> Result<()> {
        self.memory
            .write_data(0, address as _, value as _, 32)
            .context(DssError {})?;
        Ok(())
    }

    fn dss_write_datas(&self, address: u32, values: &[u8]) -> Result<()> {
        let datas: Vec<_> = values.iter().map(|n| *n as _).collect();
        self.memory
            .write_datas(0, address as _, &datas, 8)
            .context(DssError {})?;
        Ok(())
    }

    fn dss_read_data(&self, address: u32) -> Result<u32> {
        let data = self
            .memory
            .read_data(0, address as _, 32, false as _)
            .context(DssError {})?;
        Ok(data as _)
    }

    fn dss_read_datas(&self, address: u32, size: u32) -> Result<Vec<u8>> {
        let datas = self
            .memory
            .read_datas(0, address as _, 8, size as _, false as _)
            .context(DssError {})?;
        let values = datas.iter().map(|n| *n as _).collect();
        Ok(values)
    }

    fn dss_load_raw(&self, file_name: &str) -> Result<()> {
        self.memory
            .load_raw(0, SRAM_START as _, file_name, 32, false as _)
            .context(DssError {})?;
        Ok(())
    }

    fn dss_write_register(&self, register: Register, value: u32) -> Result<()> {
        self.memory
            .write_register(register, value as _)
            .context(DssError {})?;
        Ok(())
    }

    fn create_firmware_binary(device: Device) -> Result<TempPath> {
        let asset = assets::get_firmware(device)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Firmware asset not found"))
            .context(FirmwareAsset {})?;

        let mut firmware = tempfile::Builder::new()
            .prefix("flash-rover.fw.")
            .suffix(".bin")
            .tempfile()
            .context(FirmwareAsset {})?;
        firmware.write_all(&asset).context(FirmwareAsset {})?;
        let (file, path) = firmware.into_parts();
        // Drop file in order to ensure file is closed and written changes are
        // saved
        drop(file);

        Ok(path)
    }
}
