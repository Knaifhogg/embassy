#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

// test is targeted for nucleo-g070RB board
// this test will only respond to address  0x10, master read with 0xFF03

use core::fmt::{self, Write};

use embassy_executor::Spawner;
use embassy_stm32::dma::NoDma;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::i2c::{Error, I2c};
use embassy_stm32::pac::i2c::vals;
use embassy_stm32::time::Hertz;
use embassy_stm32::usart::UartTx;
use embassy_stm32::{bind_interrupts, i2c, peripherals, usart};
use embassy_time::{Duration, Timer};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    I2C1 => i2c::InterruptHandler<peripherals::I2C1>;
    USART1 => usart::InterruptHandler<peripherals::USART1>;
});

macro_rules! checkIsWrite {
    ($writer:ident, $direction:ident) => {
        match $direction {
            vals::Dir::WRITE => (),
            _ => {
                write!($writer, "Error incorrect direction {:?}\r", $direction as usize).unwrap();
                continue;
            }
        }
    };
}
macro_rules! checkIsRead {
    ($writer:ident, $direction:ident) => {
        match $direction {
            vals::Dir::READ => (),
            _ => {
                write!($writer, "Error incorrect direction {:?}\r", $direction as usize).unwrap();
                continue;
            }
        }
    };
}

pub struct SerialWriter {
    tx: UartTx<'static, peripherals::USART1, peripherals::DMA1_CH1>,
}
impl SerialWriter {
    pub fn new(tx: UartTx<'static, peripherals::USART1, peripherals::DMA1_CH1>) -> Self {
        SerialWriter { tx }
    }
}
impl fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        _ = self.tx.blocking_write(s.as_bytes());
        Ok(())
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());
    let led = Output::new(p.PA5, Level::High, Speed::Low);

    let uart = usart::Uart::new(
        p.USART1,
        p.PB7,
        p.PB6,
        Irqs,
        p.DMA1_CH1,
        p.DMA1_CH2,
        usart::Config::default(),
    )
    .unwrap();

    /*
      let uart = usart::Uart::new(
          p.USART2,
          p.PA3,
          p.PA2,
          Irqs,
          p.DMA1_CH1,
          p.DMA1_CH2,
          usart::Config::default(),
      )
      .unwrap();
    */
    let (tx, _rx) = uart.split();

    let mut writer = SerialWriter::new(tx);

    writeln!(
        &mut writer,
        "i2c slave test for arbitration lost. Will respond to address 0x10\r"
    )
    .unwrap();

    let mut config = i2c::Config::default();
    config.slave_address_7bits(0x10); // for arbitration lost test

    let i2c = I2c::new(p.I2C1, p.PB8, p.PB9, Irqs, NoDma, NoDma, Hertz(100_000), config);

    let mut buf_2 = [0; 2];
    let mut address = 0;
    let mut dir = vals::Dir::READ;
    let mut counter = 0;
    let mut result: Option<Error> = None;

    // start of the actual test
    i2c.slave_start_listen().unwrap();
    loop {
        counter += 1;
        writeln!(&mut writer, "Loop: {}\r", counter).unwrap();

        // content for test 0x10
        buf_2[0] = 0xFF;
        buf_2[1] = 0x03;
        _ = i2c.slave_write_buffer(&mut buf_2, i2c::AddressType::MainAddress);

        writeln!(&mut writer, "Waiting for master activity\r").unwrap();

        let (address, dir, size, result) = i2c.slave_transaction().await;
        writeln!(
            &mut writer,
            "Address: x{:2x}  dir: {:?} size: x{:2x}, Result:{:?}\r",
            address, dir as u8, size, result
        )
        .unwrap();

        match address {
            0x10 => {
                // Arbitration lost test Master does read 2 bytes on address 0x10
                // this slave will send 0xFF03, the other slave will send 0xFF04
                // This slave should win , so no error here
                writeln!(&mut writer, "Evaluate arbitration lost test 0x10.\r\n").unwrap();
                checkIsRead!(writer, dir);
                match result {
                    None => {
                        writeln!(&mut writer, "Test 0x10 Passed\n\r").unwrap();
                    }
                    Some(err) => writeln!(&mut writer, "Test 0x10 Failed. Error: {:?}\r", err).unwrap(),
                };
                writeln!(&mut writer, "-----\r").unwrap();
            }

            _ => (),
        }
    }
}
