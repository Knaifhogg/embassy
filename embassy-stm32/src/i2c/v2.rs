use core::cmp;
use core::future::poll_fn;
use core::task::Poll;

use cortex_m::asm::nop;
use embassy_embedded_hal::SetConfig;
use embassy_hal_internal::drop::OnDrop;
use embedded_hal_1::i2c::Operation;
use futures_util::FutureExt;
use stm32_metapac::i2c::vals::Addmode;

use super::*;
use crate::dma::{Transfer, TransferOptions};
use crate::pac::i2c;

pub(crate) unsafe fn on_interrupt<T: Instance>() {
    let regs = T::regs();
    let isr = regs.isr().read();

    if isr.tcr() || isr.tc() || isr.addr() || isr.stopf() | isr.ovr() {
        T::state().waker.wake();
    }
    // The flag can only be cleared by writting to nbytes, we won't do that here, so disable
    // the interrupt
    critical_section::with(|_| {
        regs.cr1().modify(|w| {
            w.set_tcie(false);
            w.set_addrie(false);
            w.set_stopie(false);
        });
    });
}

impl<'d, T: Instance, M: Mode> I2c<'d, T, M> {
    pub(crate) fn init(&mut self, freq: Hertz, _config: Config) {
        T::regs().cr1().modify(|reg| {
            reg.set_pe(false);
            reg.set_anfoff(false);
        });

        let timings = Timings::new(T::frequency(), freq);

        T::regs().timingr().write(|reg| {
            reg.set_presc(timings.prescale);
            reg.set_scll(timings.scll);
            reg.set_sclh(timings.sclh);
            reg.set_sdadel(timings.sdadel);
            reg.set_scldel(timings.scldel);
        });

        T::regs().cr1().modify(|reg| {
            reg.set_pe(true);
        });
    }

    fn master_stop(&mut self) {
        T::regs().cr2().write(|w| w.set_stop(true));
    }

    fn master_read(
        address: u8,
        length: usize,
        stop: Stop,
        reload: bool,
        restart: bool,
        timeout: Timeout,
    ) -> Result<(), Error> {
        assert!(length < 256);

        if !restart {
            // Wait for any previous address sequence to end
            // automatically. This could be up to 50% of a bus
            // cycle (ie. up to 0.5/freq)
            while T::regs().cr2().read().start() {
                timeout.check()?;
            }
        }

        // Set START and prepare to receive bytes into
        // `buffer`. The START bit can be set even if the bus
        // is BUSY or I2C is in slave mode.

        let reload = if reload {
            i2c::vals::Reload::NOTCOMPLETED
        } else {
            i2c::vals::Reload::COMPLETED
        };

        T::regs().cr2().modify(|w| {
            w.set_sadd((address << 1) as u16);
            w.set_add10(Addmode::BIT7);
            w.set_dir(i2c::vals::Dir::READ);
            w.set_nbytes(length as u8);
            w.set_start(true);
            w.set_autoend(stop.autoend());
            w.set_reload(reload);
        });

        Ok(())
    }

    fn master_write(address: u8, length: usize, stop: Stop, reload: bool, timeout: Timeout) -> Result<(), Error> {
        assert!(length < 256);

        // Wait for any previous address sequence to end
        // automatically. This could be up to 50% of a bus
        // cycle (ie. up to 0.5/freq)
        while T::regs().cr2().read().start() {
            timeout.check()?;
        }

        let reload = if reload {
            i2c::vals::Reload::NOTCOMPLETED
        } else {
            i2c::vals::Reload::COMPLETED
        };

        // Set START and prepare to send `bytes`. The
        // START bit can be set even if the bus is BUSY or
        // I2C is in slave mode.
        T::regs().cr2().modify(|w| {
            w.set_sadd((address << 1) as u16);
            w.set_add10(Addmode::BIT7);
            w.set_dir(i2c::vals::Dir::WRITE);
            w.set_nbytes(length as u8);
            w.set_start(true);
            w.set_autoend(stop.autoend());
            w.set_reload(reload);
        });

        Ok(())
    }

    fn master_continue(length: usize, reload: bool, timeout: Timeout) -> Result<(), Error> {
        assert!(length < 256 && length > 0);

        while !T::regs().isr().read().tcr() {
            timeout.check()?;
        }

        let reload = if reload {
            i2c::vals::Reload::NOTCOMPLETED
        } else {
            i2c::vals::Reload::COMPLETED
        };

        T::regs().cr2().modify(|w| {
            w.set_nbytes(length as u8);
            w.set_reload(reload);
        });

        Ok(())
    }

    fn flush_txdr(&self) {
        if T::regs().isr().read().txis() {
            T::regs().txdr().write(|w| w.set_txdata(0));
        }
        if !T::regs().isr().read().txe() {
            T::regs().isr().modify(|w| w.set_txe(true))
        }
    }

    fn wait_txe(&self, timeout: Timeout) -> Result<(), Error> {
        loop {
            let isr = T::regs().isr().read();
            if isr.txe() {
                return Ok(());
            } else if isr.berr() {
                T::regs().icr().write(|reg| reg.set_berrcf(true));
                return Err(Error::Bus);
            } else if isr.arlo() {
                T::regs().icr().write(|reg| reg.set_arlocf(true));
                return Err(Error::Arbitration);
            } else if isr.nackf() {
                T::regs().icr().write(|reg| reg.set_nackcf(true));
                self.flush_txdr();
                return Err(Error::Nack);
            }

            timeout.check()?;
        }
    }

    fn wait_rxne(&self, timeout: Timeout) -> Result<(), Error> {
        loop {
            let isr = T::regs().isr().read();
            if isr.rxne() {
                return Ok(());
            } else if isr.berr() {
                T::regs().icr().write(|reg| reg.set_berrcf(true));
                return Err(Error::Bus);
            } else if isr.arlo() {
                T::regs().icr().write(|reg| reg.set_arlocf(true));
                return Err(Error::Arbitration);
            } else if isr.nackf() {
                T::regs().icr().write(|reg| reg.set_nackcf(true));
                self.flush_txdr();
                return Err(Error::Nack);
            }

            timeout.check()?;
        }
    }

    fn wait_tc(&self, timeout: Timeout) -> Result<(), Error> {
        loop {
            let isr = T::regs().isr().read();
            if isr.tc() {
                return Ok(());
            } else if isr.berr() {
                T::regs().icr().write(|reg| reg.set_berrcf(true));
                return Err(Error::Bus);
            } else if isr.arlo() {
                T::regs().icr().write(|reg| reg.set_arlocf(true));
                return Err(Error::Arbitration);
            } else if isr.nackf() {
                T::regs().icr().write(|reg| reg.set_nackcf(true));
                self.flush_txdr();
                return Err(Error::Nack);
            }

            timeout.check()?;
        }
    }

    fn read_internal(&mut self, address: u8, read: &mut [u8], restart: bool, timeout: Timeout) -> Result<(), Error> {
        let completed_chunks = read.len() / 255;
        let total_chunks = if completed_chunks * 255 == read.len() {
            completed_chunks
        } else {
            completed_chunks + 1
        };
        let last_chunk_idx = total_chunks.saturating_sub(1);

        Self::master_read(
            address,
            read.len().min(255),
            Stop::Automatic,
            last_chunk_idx != 0,
            restart,
            timeout,
        )?;

        for (number, chunk) in read.chunks_mut(255).enumerate() {
            if number != 0 {
                Self::master_continue(chunk.len(), number != last_chunk_idx, timeout)?;
            }

            for byte in chunk {
                // Wait until we have received something
                self.wait_rxne(timeout)?;

                *byte = T::regs().rxdr().read().rxdata();
            }
        }
        Ok(())
    }

    fn write_internal(&mut self, address: u8, write: &[u8], send_stop: bool, timeout: Timeout) -> Result<(), Error> {
        let completed_chunks = write.len() / 255;
        let total_chunks = if completed_chunks * 255 == write.len() {
            completed_chunks
        } else {
            completed_chunks + 1
        };
        let last_chunk_idx = total_chunks.saturating_sub(1);

        // I2C start
        //
        // ST SAD+W
        if let Err(err) = Self::master_write(
            address,
            write.len().min(255),
            Stop::Software,
            last_chunk_idx != 0,
            timeout,
        ) {
            if send_stop {
                self.master_stop();
            }
            return Err(err);
        }

        for (number, chunk) in write.chunks(255).enumerate() {
            if number != 0 {
                Self::master_continue(chunk.len(), number != last_chunk_idx, timeout)?;
            }

            for byte in chunk {
                // Wait until we are allowed to send data
                // (START has been ACKed or last byte when
                // through)
                if let Err(err) = self.wait_txe(timeout) {
                    if send_stop {
                        self.master_stop();
                    }
                    return Err(err);
                }

                T::regs().txdr().write(|w| w.set_txdata(*byte));
            }
        }
        // Wait until the write finishes
        let result = self.wait_tc(timeout);
        if send_stop {
            self.master_stop();
        }
        result
    }

    // =========================
    //  Blocking public API

    /// Blocking read.
    pub fn blocking_read(&mut self, address: u8, read: &mut [u8]) -> Result<(), Error> {
        self.read_internal(address, read, false, self.timeout())
        // Automatic Stop
    }

    /// Blocking write.
    pub fn blocking_write(&mut self, address: u8, write: &[u8]) -> Result<(), Error> {
        self.write_internal(address, write, true, self.timeout())
    }

    /// Blocking write, restart, read.
    pub fn blocking_write_read(&mut self, address: u8, write: &[u8], read: &mut [u8]) -> Result<(), Error> {
        let timeout = self.timeout();
        self.write_internal(address, write, false, timeout)?;
        self.read_internal(address, read, true, timeout)
        // Automatic Stop
    }

    /// Blocking transaction with operations.
    ///
    /// Consecutive operations of same type are merged. See [transaction contract] for details.
    ///
    /// [transaction contract]: embedded_hal_1::i2c::I2c::transaction
    pub fn blocking_transaction(&mut self, addr: u8, operations: &mut [Operation<'_>]) -> Result<(), Error> {
        let _ = addr;
        let _ = operations;
        todo!()
    }

    /// Blocking write multiple buffers.
    ///
    /// The buffers are concatenated in a single write transaction.
    pub fn blocking_write_vectored(&mut self, address: u8, write: &[&[u8]]) -> Result<(), Error> {
        if write.is_empty() {
            return Err(Error::ZeroLengthTransfer);
        }

        let timeout = self.timeout();

        let first_length = write[0].len();
        let last_slice_index = write.len() - 1;

        if let Err(err) = Self::master_write(
            address,
            first_length.min(255),
            Stop::Software,
            (first_length > 255) || (last_slice_index != 0),
            timeout,
        ) {
            self.master_stop();
            return Err(err);
        }

        for (idx, slice) in write.iter().enumerate() {
            let slice_len = slice.len();
            let completed_chunks = slice_len / 255;
            let total_chunks = if completed_chunks * 255 == slice_len {
                completed_chunks
            } else {
                completed_chunks + 1
            };
            let last_chunk_idx = total_chunks.saturating_sub(1);

            if idx != 0 {
                if let Err(err) = Self::master_continue(
                    slice_len.min(255),
                    (idx != last_slice_index) || (slice_len > 255),
                    timeout,
                ) {
                    self.master_stop();
                    return Err(err);
                }
            }

            for (number, chunk) in slice.chunks(255).enumerate() {
                if number != 0 {
                    if let Err(err) = Self::master_continue(
                        chunk.len(),
                        (number != last_chunk_idx) || (idx != last_slice_index),
                        timeout,
                    ) {
                        self.master_stop();
                        return Err(err);
                    }
                }

                for byte in chunk {
                    // Wait until we are allowed to send data
                    // (START has been ACKed or last byte when
                    // through)
                    if let Err(err) = self.wait_txe(timeout) {
                        self.master_stop();
                        return Err(err);
                    }

                    // Put byte on the wire
                    //self.i2c.txdr.write(|w| w.txdata().bits(*byte));
                    T::regs().txdr().write(|w| w.set_txdata(*byte));
                }
            }
        }
        // Wait until the write finishes
        let result = self.wait_tc(timeout);
        self.master_stop();
        result
    }
}

impl<'d, T: Instance> I2c<'d, T, Async> {
    async fn write_dma_internal(
        &mut self,
        address: u8,
        write: &[u8],
        first_slice: bool,
        last_slice: bool,
        timeout: Timeout,
    ) -> Result<(), Error> {
        let total_len = write.len();

        let dma_transfer = unsafe {
            let regs = T::regs();
            regs.cr1().modify(|w| {
                w.set_txdmaen(true);
                if first_slice {
                    w.set_tcie(true);
                }
            });
            let dst = regs.txdr().as_ptr() as *mut u8;

            self.tx_dma.as_mut().unwrap().write(write, dst, Default::default())
        };

        let state = T::state();
        let mut remaining_len = total_len;

        let on_drop = OnDrop::new(|| {
            let regs = T::regs();
            regs.cr1().modify(|w| {
                if last_slice {
                    w.set_txdmaen(false);
                }
                w.set_tcie(false);
            })
        });

        poll_fn(|cx| {
            state.waker.register(cx.waker());

            let isr = T::regs().isr().read();
            if remaining_len == total_len {
                if first_slice {
                    Self::master_write(
                        address,
                        total_len.min(255),
                        Stop::Software,
                        (total_len > 255) || !last_slice,
                        timeout,
                    )?;
                } else {
                    Self::master_continue(total_len.min(255), (total_len > 255) || !last_slice, timeout)?;
                    T::regs().cr1().modify(|w| w.set_tcie(true));
                }
            } else if !(isr.tcr() || isr.tc()) {
                // poll_fn was woken without an interrupt present
                return Poll::Pending;
            } else if remaining_len == 0 {
                return Poll::Ready(Ok(()));
            } else {
                let last_piece = (remaining_len <= 255) && last_slice;

                if let Err(e) = Self::master_continue(remaining_len.min(255), !last_piece, timeout) {
                    return Poll::Ready(Err(e));
                }
                T::regs().cr1().modify(|w| w.set_tcie(true));
            }

            remaining_len = remaining_len.saturating_sub(255);
            Poll::Pending
        })
        .await?;

        dma_transfer.await;

        if last_slice {
            // This should be done already
            self.wait_tc(timeout)?;
            self.master_stop();
        }

        drop(on_drop);

        Ok(())
    }

    async fn read_dma_internal(
        &mut self,
        address: u8,
        buffer: &mut [u8],
        restart: bool,
        timeout: Timeout,
    ) -> Result<(), Error> {
        let total_len = buffer.len();

        let dma_transfer = unsafe {
            let regs = T::regs();
            regs.cr1().modify(|w| {
                w.set_rxdmaen(true);
                w.set_tcie(true);
            });
            let src = regs.rxdr().as_ptr() as *mut u8;

            self.rx_dma.as_mut().unwrap().read(src, buffer, Default::default())
        };

        let state = T::state();
        let mut remaining_len = total_len;

        let on_drop = OnDrop::new(|| {
            let regs = T::regs();
            regs.cr1().modify(|w| {
                w.set_rxdmaen(false);
                w.set_tcie(false);
            })
        });

        poll_fn(|cx| {
            state.waker.register(cx.waker());

            let isr = T::regs().isr().read();
            if remaining_len == total_len {
                Self::master_read(
                    address,
                    total_len.min(255),
                    Stop::Software,
                    total_len > 255,
                    restart,
                    timeout,
                )?;
            } else if !(isr.tcr() || isr.tc()) {
                // poll_fn was woken without an interrupt present
                return Poll::Pending;
            } else if remaining_len == 0 {
                return Poll::Ready(Ok(()));
            } else {
                let last_piece = remaining_len <= 255;

                if let Err(e) = Self::master_continue(remaining_len.min(255), !last_piece, timeout) {
                    return Poll::Ready(Err(e));
                }
                T::regs().cr1().modify(|w| w.set_tcie(true));
            }

            remaining_len = remaining_len.saturating_sub(255);
            Poll::Pending
        })
        .await?;

        dma_transfer.await;

        // This should be done already
        self.wait_tc(timeout)?;
        self.master_stop();

        drop(on_drop);

        Ok(())
    }

    // =========================
    //  Async public API

    /// Write.
    pub async fn write(&mut self, address: u8, write: &[u8]) -> Result<(), Error> {
        let timeout = self.timeout();
        if write.is_empty() {
            self.write_internal(address, write, true, timeout)
        } else {
            timeout
                .with(self.write_dma_internal(address, write, true, true, timeout))
                .await
        }
    }

    /// Write multiple buffers.
    ///
    /// The buffers are concatenated in a single write transaction.
    pub async fn write_vectored(&mut self, address: u8, write: &[&[u8]]) -> Result<(), Error> {
        let timeout = self.timeout();

        if write.is_empty() {
            return Err(Error::ZeroLengthTransfer);
        }
        let mut iter = write.iter();

        let mut first = true;
        let mut current = iter.next();
        while let Some(c) = current {
            let next = iter.next();
            let is_last = next.is_none();

            let fut = self.write_dma_internal(address, c, first, is_last, timeout);
            timeout.with(fut).await?;
            first = false;
            current = next;
        }
        Ok(())
    }

    /// Read.
    pub async fn read(&mut self, address: u8, buffer: &mut [u8]) -> Result<(), Error> {
        let timeout = self.timeout();

        if buffer.is_empty() {
            self.read_internal(address, buffer, false, timeout)
        } else {
            let fut = self.read_dma_internal(address, buffer, false, timeout);
            timeout.with(fut).await
        }
    }

    /// Write, restart, read.
    pub async fn write_read(&mut self, address: u8, write: &[u8], read: &mut [u8]) -> Result<(), Error> {
        let timeout = self.timeout();

        if write.is_empty() {
            self.write_internal(address, write, false, timeout)?;
        } else {
            let fut = self.write_dma_internal(address, write, true, true, timeout);
            timeout.with(fut).await?;
        }

        if read.is_empty() {
            self.read_internal(address, read, true, timeout)?;
        } else {
            let fut = self.read_dma_internal(address, read, true, timeout);
            timeout.with(fut).await?;
        }

        Ok(())
    }

    /// Transaction with operations.
    ///
    /// Consecutive operations of same type are merged. See [transaction contract] for details.
    ///
    /// [transaction contract]: embedded_hal_1::i2c::I2c::transaction
    pub async fn transaction(&mut self, addr: u8, operations: &mut [Operation<'_>]) -> Result<(), Error> {
        let _ = addr;
        let _ = operations;
        todo!()
    }
}

impl<'d, T: Instance, M: Mode> Drop for I2c<'d, T, M> {
    fn drop(&mut self) {
        T::disable();
    }
}

/// I2C Stop Configuration
///
/// Peripheral options for generating the STOP condition
#[derive(Copy, Clone, PartialEq)]
enum Stop {
    /// Software end mode: Must write register to generate STOP condition
    Software,
    /// Automatic end mode: A STOP condition is automatically generated once the
    /// configured number of bytes have been transferred
    Automatic,
}

impl Stop {
    fn autoend(&self) -> i2c::vals::Autoend {
        match self {
            Stop::Software => i2c::vals::Autoend::SOFTWARE,
            Stop::Automatic => i2c::vals::Autoend::AUTOMATIC,
        }
    }
}

struct Timings {
    prescale: u8,
    scll: u8,
    sclh: u8,
    sdadel: u8,
    scldel: u8,
}

impl Timings {
    fn new(i2cclk: Hertz, freq: Hertz) -> Self {
        let i2cclk = i2cclk.0;
        let freq = freq.0;
        // Refer to RM0433 Rev 7 Figure 539 for setup and hold timing:
        //
        // t_I2CCLK = 1 / PCLK1
        // t_PRESC  = (PRESC + 1) * t_I2CCLK
        // t_SCLL   = (SCLL + 1) * t_PRESC
        // t_SCLH   = (SCLH + 1) * t_PRESC
        //
        // t_SYNC1 + t_SYNC2 > 4 * t_I2CCLK
        // t_SCL ~= t_SYNC1 + t_SYNC2 + t_SCLL + t_SCLH
        let ratio = i2cclk / freq;

        // For the standard-mode configuration method, we must have a ratio of 4
        // or higher
        assert!(ratio >= 4, "The I2C PCLK must be at least 4 times the bus frequency!");

        let (presc_reg, scll, sclh, sdadel, scldel) = if freq > 100_000 {
            // Fast-mode (Fm) or Fast-mode Plus (Fm+)
            // here we pick SCLL + 1 = 2 * (SCLH + 1)

            // Prescaler, 384 ticks for sclh/scll. Round up then subtract 1
            let presc_reg = ((ratio - 1) / 384) as u8;
            // ratio < 1200 by pclk 120MHz max., therefore presc < 16

            // Actual precale value selected
            let presc = (presc_reg + 1) as u32;

            let sclh = ((ratio / presc) - 3) / 3;
            let scll = (2 * (sclh + 1)) - 1;

            let (sdadel, scldel) = if freq > 400_000 {
                // Fast-mode Plus (Fm+)
                assert!(i2cclk >= 17_000_000); // See table in datsheet

                let sdadel = i2cclk / 8_000_000 / presc;
                let scldel = i2cclk / 4_000_000 / presc - 1;

                (sdadel, scldel)
            } else {
                // Fast-mode (Fm)
                assert!(i2cclk >= 8_000_000); // See table in datsheet

                let sdadel = i2cclk / 4_000_000 / presc;
                let scldel = i2cclk / 2_000_000 / presc - 1;

                (sdadel, scldel)
            };

            (presc_reg, scll as u8, sclh as u8, sdadel as u8, scldel as u8)
        } else {
            // Standard-mode (Sm)
            // here we pick SCLL = SCLH
            assert!(i2cclk >= 2_000_000); // See table in datsheet

            // Prescaler, 512 ticks for sclh/scll. Round up then
            // subtract 1
            let presc = (ratio - 1) / 512;
            let presc_reg = cmp::min(presc, 15) as u8;

            // Actual prescale value selected
            let presc = (presc_reg + 1) as u32;

            let sclh = ((ratio / presc) - 2) / 2;
            let scll = sclh;

            // Speed check
            assert!(sclh < 256, "The I2C PCLK is too fast for this bus frequency!");

            let sdadel = i2cclk / 2_000_000 / presc;
            let scldel = i2cclk / 500_000 / presc - 1;

            (presc_reg, scll as u8, sclh as u8, sdadel as u8, scldel as u8)
        };

        // Sanity check
        assert!(presc_reg < 16);

        // Keep values within reasonable limits for fast per_ck
        let sdadel = cmp::max(sdadel, 2);
        let scldel = cmp::max(scldel, 4);

        //(presc_reg, scll, sclh, sdadel, scldel)
        Self {
            prescale: presc_reg,
            scll,
            sclh,
            sdadel,
            scldel,
        }
    }
}

impl<'d, T: Instance, M: Mode> SetConfig for I2c<'d, T, M> {
    type Config = Hertz;
    type ConfigError = ();
    fn set_config(&mut self, config: &Self::Config) -> Result<(), ()> {
        let timings = Timings::new(T::frequency(), *config);
        T::regs().timingr().write(|reg| {
            reg.set_presc(timings.prescale);
            reg.set_scll(timings.scll);
            reg.set_sclh(timings.sclh);
            reg.set_sdadel(timings.sdadel);
            reg.set_scldel(timings.scldel);
        });

        Ok(())
    }
}

impl<'d, T: Instance, M: Mode> SetConfig for I2cSlave<'d, T, M> {
    type Config = Hertz;
    type ConfigError = ();
    fn set_config(&mut self, config: &Self::Config) -> Result<(), ()> {
        let timings = Timings::new(T::frequency(), *config);
        T::regs().timingr().write(|reg| {
            reg.set_presc(timings.prescale);
            reg.set_sdadel(timings.sdadel);
            reg.set_scldel(timings.scldel);
        });

        Ok(())
    }
}

// I2cSlave methods
impl<'d, T: Instance, M: Mode> I2cSlave<'d, T, M> {
    pub(crate) fn init(&mut self, freq: Hertz, config: SlaveConfig) {
        T::regs().cr1().modify(|reg| {
            reg.set_pe(false);
            reg.set_anfoff(false);
        });

        let timings = Timings::new(T::frequency(), freq);

        T::regs().timingr().write(|reg| {
            reg.set_presc(timings.prescale);
            reg.set_sdadel(timings.sdadel);
            reg.set_scldel(timings.scldel);
        });

        T::regs().cr1().modify(|reg| {
            reg.set_nostretch(false);
            reg.set_gcen(config.general_call);
            reg.set_sbc(true);
            reg.set_pe(true);
        });

        self.reconfigure_addresses(config.addr);
    }

    pub fn reconfigure_addresses(&mut self, addresses: OwnAddresses) {
        match addresses {
            OwnAddresses::OA1(oa1) => Self::configure_oa1(oa1),
            OwnAddresses::OA2(oa2) => Self::configure_oa2(oa2),
            OwnAddresses::Both { oa1, oa2 } => {
                Self::configure_oa1(oa1);
                Self::configure_oa2(oa2);
            }
        }
    }

    fn configure_oa1(oa1: OwnAddress) {
        match oa1 {
            OwnAddress::SevenBit(addr) => T::regs().oar1().write(|reg| {
                reg.set_oa1en(false);
                reg.set_oa1((addr << 1) as u16);
                reg.set_oa1mode(Addmode::BIT7);
                reg.set_oa1en(true);
            }),
            OwnAddress::TenBit(addr) => T::regs().oar1().write(|reg| {
                reg.set_oa1en(false);
                reg.set_oa1(addr);
                reg.set_oa1mode(Addmode::BIT10);
                reg.set_oa1en(true);
            }),
        }
    }

    fn configure_oa2(oa2: OA2) {
        T::regs().oar2().write(|reg| {
            reg.set_oa2en(false);
            reg.set_oa2msk(oa2.mask.into());
            reg.set_oa2(oa2.addr << 1);
            reg.set_oa2en(true);
        });
    }

    /// Listen for incoming I2C messages.
    ///
    /// `size_to_receive` is a function that takes the matched address as an argument and returns the number of bytes that are expected to be received from the master.
    pub async fn listen(&mut self) -> Result<Command, Error> {
        let state = T::state();

        T::regs().cr1().modify(|reg| {
            reg.set_addrie(true);
        });

        poll_fn(|cx| {
            state.waker.register(cx.waker());
            let isr = T::regs().isr().read();
            if !isr.addr() {
                return Poll::Pending;
            } else {
                match isr.dir() {
                    i2c::vals::Dir::WRITE => {
                        // this clear to the address flag can be done as it won't affect the dma read.
                        T::regs().icr().write(|reg| reg.set_addrcf(true));
                        return Poll::Ready(Ok(Command {
                            kind: CommandKind::SlaveReceive,
                            address: self.determine_matched_address()?,
                        }));
                    }
                    i2c::vals::Dir::READ => {
                        // we do not clear the address flag here as it will be cleared by the dma write
                        // if we clear it here the clock stretching will stop and the master will read in bad data this slave is ready to send it
                        return Poll::Ready(Ok(Command {
                            kind: CommandKind::SlaveSend,
                            address: self.determine_matched_address()?,
                        }));
                    }
                }
            }
        })
        .await
    }

    fn determine_matched_address(&self) -> Result<OwnAddress, Error> {
        let matched = T::regs().isr().read().addcode();
        let address = if matched >> 3 == 0b11110 {
            // is 10-bit address and we need to get the other 8 bits from the rxdr
            // we do this by doing a blocking read of 1 byte
            let mut buffer = [0];
            Self::blocking_read(&mut buffer, self.timeout())?;
            Ok(OwnAddress::TenBit((matched as u16) << 6 | buffer[0] as u16))
        } else {
            Ok(OwnAddress::SevenBit(matched))
        };

        address
    }

    fn blocking_read(buffer: &mut [u8], timeout: Timeout) -> Result<(), Error> {
        for byte in buffer.iter_mut() {
            while !T::regs().isr().read().rxne() {
                timeout.check()?;
            }
            *byte = T::regs().rxdr().read().rxdata();
        }
        Ok(())
    }
}

impl<'d, T: Instance> I2cSlave<'d, T, Async> {
    pub async fn respond_to_receive(&mut self, buffer: &mut [u8]) -> Result<ReceiveStatus, Error> {

        let timeout = self.timeout();
        let size = timeout.with(self.read_dma_internal(buffer, timeout)).await?;

        T::regs().cr1().modify(|reg| {
            reg.set_addrie(true);
        });

        let state = T::state();

        let timeout = self.timeout();

        let send_requested = timeout
            .with(poll_fn(|cx| {
                state.waker.register(cx.waker());


                let isr = T::regs().isr().read();
                if isr.addr() {
                    if isr.dir() == i2c::vals::Dir::READ {
                        return Poll::Ready(Ok(ReceiveStatus::SendRequested(size)));
                    }

                    return Poll::Ready(Ok(ReceiveStatus::Done(size)));
                };

                return Poll::Pending;
            }))
            .await;

        match send_requested {
            Ok(ReceiveStatus::SendRequested(size)) => Ok(ReceiveStatus::SendRequested(size)),
            Ok(ReceiveStatus::Done(_)) | Err(_) => Ok(ReceiveStatus::Done(size)),
        }
    }

    pub async fn respond_to_send(&mut self, buffer: &[u8]) -> Result<SendStatus, Error> {
        let timeout = self.timeout();
        timeout.with(self.write_dma_internal(buffer, timeout, false)).await
    }

    // pub async fn respond_and_fill(&mut self, buffer: &[u8], fill: u8) -> Result<SendStatus, Error> {
    //     let resp_stat = self.respond_to_send(buffer).await?;


    //     let res = if matches!(resp_stat, SendStatus::MoreBytesRequested) {
    //         self.write_dma_internal(&[fill], self.timeout(), true).await?;
    //         Ok(SendStatus::Done)
    //     } else {
    //         Ok(resp_stat)
    //     };

    //     res
    // }

    // for data reception in slave mode
    async fn read_dma_internal(&mut self, buffer: &mut [u8], timeout: Timeout) -> Result<usize, Error> {
        let total_len = buffer.len();

        let mut dma_transfer = unsafe {
            let regs = T::regs();
            regs.cr1().modify(|w| {
                w.set_rxdmaen(true);
                w.set_stopie(true);
            });
            let src = regs.rxdr().as_ptr() as *mut u8;

            self.rx_dma.as_mut().unwrap().read(src, buffer, Default::default())
        };
        T::regs().icr().write(|reg| {
            reg.set_addrcf(true);
        });

        let state = T::state();
        let mut remaining_len = total_len;

        let on_drop = OnDrop::new(|| {
            let regs = T::regs();
            regs.cr1().modify(|w| {
                w.set_rxdmaen(false);
                w.set_stopie(false);
            })
        });

        let total_received = poll_fn(|cx| {
            state.waker.register(cx.waker());


            let isr = T::regs().isr().read();
            if isr.stopf() {
                T::regs().icr().write(|reg| reg.set_stopcf(true));
                // if we get a stop condition we need to determine how many bytes were received
                let poll = Poll::Ready(Ok(total_len - dma_transfer.get_remaining_transfers() as usize));
                dma_transfer.request_stop();
                poll
            } else {
                Poll::Pending
            }
        })
        .await?;

        // should already be finished
        dma_transfer.await;

        drop(on_drop);

        Ok(total_received)
    }
    // writing dma (mem -> peripheral) is for responding to an I2C slave read request
    // setting circular to true is used to write a single byte until the stop condition
    async fn write_dma_internal(
        &mut self,
        buffer: &[u8],
        timeout: Timeout,
        circular: bool,
    ) -> Result<SendStatus, Error> {
        let total_len = if circular { 255 } else { buffer.len() };



        let timeout = self.timeout();

        let mut dma_transfer = unsafe {
            let regs = T::regs();
            regs.cr2().modify(|w| w.set_nbytes(total_len as u8));
            regs.cr1().modify(|w| {
                w.set_txdmaen(true);
                w.set_txie(true);
                w.set_stopie(true);
            });
            let dst = regs.txdr().as_ptr() as *mut u8;

            let transfer_options = TransferOptions {
                circular,
                ..Default::default()
            };
            self.tx_dma.as_mut().unwrap().write(buffer, dst, transfer_options)
        };

        // this clear to the address flag is necessary to start the transfer
        // It must be done after the dma transfer is set up
        T::regs().icr().write(|reg| {
            reg.set_addrcf(true);
        });
        // if we are in circular mode we need to set the NACK bit to false if it was set to true before
        T::regs().cr2().modify(|reg| reg.set_nack(false));

        let state = T::state();
        let mut remaining_len = total_len;

        let on_drop = OnDrop::new(|| {
            let regs = T::regs();
            regs.cr1().modify(|w| {
                w.set_txdmaen(false);
                w.set_txie(false);
                w.set_stopie(false);
            })
        });

        let size = poll_fn(|cx| {
            state.waker.register(cx.waker());

            // check ovr


            let isr = T::regs().isr().read();
            if isr.stopf() {
                T::regs().icr().write(|reg| reg.set_stopcf(true));
                let remaining = dma_transfer.get_remaining_transfers();

                if remaining > 0 {
                    dma_transfer.request_stop();
                    Poll::Ready(Ok(SendStatus::LeftoverBytes(remaining as usize)))
                } else {
                    Poll::Ready(Ok(SendStatus::Done))
                }
            } else {

                if let Poll::Ready(_) = dma_transfer.poll_unpin(cx) {
                    T::regs().cr2().modify(|reg| reg.set_nack(true));
                    return Poll::Ready(Ok(SendStatus::MoreBytesRequested));
                };
                Poll::Pending
            }
        })
        .await?;

        dma_transfer.await;

        drop(on_drop);

        Ok(size)
    }
}
