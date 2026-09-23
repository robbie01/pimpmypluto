//! Safe-ish face of the AD936x: ADI's no-OS driver does the work, this module
//! owns the handle and knows what is particular to this board.

use crate::noos::*;
use std::fmt;

/// EMIO GPIO 13 (Zynq GPIO line 54 + 13).
const GPIO_RESETB: i32 = 67;
const ADC_CORE: u32 = 0x7902_0000;
const DAC_CORE: u32 = 0x7902_4000;

#[derive(Debug)]
pub struct Error(pub &'static str, pub i32);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed ({})", self.0, self.1)
    }
}
impl std::error::Error for Error {}

fn ck(what: &'static str, rc: i32) -> Result<(), Error> {
    if rc < 0 { Err(Error(what, rc)) } else { Ok(()) }
}

pub struct Radio {
    phy: *mut ad9361_rf_phy,
}
// The driver keeps no thread-local state; callers serialise access via &mut.
unsafe impl Send for Radio {}

impl Radio {
    /// Bring the transceiver up from reset with this board's interface
    /// settings.  Values mirror the stock device tree (`adi,*` properties),
    /// which is what the PCB and the bitstream's CMOS interface were tuned to.
    pub fn new() -> Result<Self, Error> {
        // SAFETY: returns a pointer to a static initialised at load time.
        let p = unsafe { &mut *pmp_default_init_param() };
        p.dev_sel = dev_id_ID_AD9363A;
        p.reference_clk_rate = 40_000_000;
        p.xo_disable_use_ext_refclk_enable = 1;
        p.two_rx_two_tx_mode_enable = 0;
        p.one_rx_one_tx_mode_use_rx_num = 1;
        p.one_rx_one_tx_mode_use_tx_num = 1;
        p.frequency_division_duplex_mode_enable = 1;
        p.tx_lo_powerdown_managed_enable = 1;
        p.rx_synthesizer_frequency_hz = 2_400_000_000;
        p.tx_synthesizer_frequency_hz = 2_450_000_000;
        p.rx_path_clock_frequencies = [983_040_000, 245_760_000, 122_880_000, 61_440_000, 30_720_000, 30_720_000];
        p.tx_path_clock_frequencies = [983_040_000, 122_880_000, 122_880_000, 61_440_000, 30_720_000, 30_720_000];
        p.rf_rx_bandwidth_hz = 18_000_000;
        p.rf_tx_bandwidth_hz = 18_000_000;
        p.rx_rf_port_input_select = 0;
        p.tx_rf_port_input_select = 0;
        p.tx_attenuation_mdB = 10_000;
        p.gc_rx1_mode = 2;
        p.gc_rx2_mode = 2;
        // Digital interface: single-ended CMOS, full port, ports swapped.
        p.lvds_mode_enable = 0;
        p.lvds_rx_onchip_termination_enable = 0;
        p.full_port_enable = 1;
        p.swap_ports_enable = 1;
        p.pp_tx_swap_enable = 1;
        p.pp_rx_swap_enable = 1;
        p.rx_frame_pulse_mode_enable = 1;
        p.rx_data_clock_delay = 0;
        p.rx_data_delay = 0;
        p.tx_fb_clock_delay = 0;
        p.tx_data_delay = 9;
        p.digital_interface_tune_skip_mode = 0;
        p.digital_interface_tune_fir_disable = 1;
        p.gpio_resetb.number = GPIO_RESETB;
        p.spi_param.max_speed_hz = 10_000_000; // ADI's table leaves this 0, which spidev rejects

        let mut phy = std::ptr::null_mut();
        // SAFETY: init param fully initialised; the driver allocates `phy`.
        ck("ad9361_init", unsafe { ad9361_init(&mut phy, p) })?;
        // ad9361_init brings up the ADC core itself (it needs it to tune the
        // digital interface) but leaves the DAC core to the caller.
        ck("axi_dac_init", unsafe { axi_dac_init(&mut (*phy).tx_dac, pmp_tx_dac_init()) })?;

        // The bitstream has four converter channels (it is a 2R2T design) and
        // its pack/unpack cores lay the DMA stream out across whichever are
        // enabled.  The no-OS init enables the channels it was told about but
        // never parks the others, and FPGA register state survives a daemon
        // restart -- so park RX2/TX2 explicitly or the stream silently
        // becomes I0,Q0,I1,Q1.
        for ch in 2..4u32 {
            let ok = crate::platform::reg_write(ADC_CORE + 0x400 + ch * 0x40, 0)   // CHAN_CNTRL: disable
                && crate::platform::reg_write(DAC_CORE + 0x418 + ch * 0x40, 3);    // CHAN_CNTRL_7: data = zero
            if !ok {
                return Err(Error("park unused converter channels", -libc::EFAULT));
            }
        }
        // Unity IQ-correction coefficients, as ADI's kernel driver leaves them.
        for ch in 0..2u32 {
            crate::platform::reg_write(DAC_CORE + 0x41c + ch * 0x40, 0x4000_0000);
        }
        crate::platform::reg_write(DAC_CORE + 0x44, 1); // SYNC: latch the DAC channel selects
        Ok(Radio { phy })
    }

    pub fn set_sample_rate(&mut self, hz: u32) -> Result<u32, Error> {
        let mut got = 0;
        // SAFETY (here and below): `phy` is valid for the life of `self`.
        unsafe {
            ck("set_tx_sampling_freq", ad9361_set_tx_sampling_freq(self.phy, hz))?;
            ck("get_tx_sampling_freq", ad9361_get_tx_sampling_freq(self.phy, &mut got))?;
        }
        Ok(got)
    }

    pub fn set_tx_lo(&mut self, hz: u64) -> Result<u64, Error> {
        let mut got = 0;
        unsafe {
            ck("set_tx_lo_freq", ad9361_set_tx_lo_freq(self.phy, hz))?;
            ck("get_tx_lo_freq", ad9361_get_tx_lo_freq(self.phy, &mut got))?;
        }
        Ok(got)
    }

    pub fn set_rx_lo(&mut self, hz: u64) -> Result<u64, Error> {
        let mut got = 0;
        unsafe {
            ck("set_rx_lo_freq", ad9361_set_rx_lo_freq(self.phy, hz))?;
            ck("get_rx_lo_freq", ad9361_get_rx_lo_freq(self.phy, &mut got))?;
        }
        Ok(got)
    }

    pub fn set_tx_bandwidth(&mut self, hz: u32) -> Result<(), Error> {
        ck("set_tx_rf_bandwidth", unsafe { ad9361_set_tx_rf_bandwidth(self.phy, hz) })
    }

    pub fn set_rx_bandwidth(&mut self, hz: u32) -> Result<(), Error> {
        ck("set_rx_rf_bandwidth", unsafe { ad9361_set_rx_rf_bandwidth(self.phy, hz) })
    }

    pub fn set_bandwidth(&mut self, hz: u32) -> Result<(), Error> {
        self.set_tx_bandwidth(hz)?;
        self.set_rx_bandwidth(hz)
    }

    /// Slow-attack automatic gain control.
    pub fn set_rx_agc(&mut self) -> Result<(), Error> {
        ck("set_rx_gain_control_mode", unsafe { ad9361_set_rx_gain_control_mode(self.phy, 0, 2) })
    }

    /// TX attenuation in milli-dB, 0 = full power.
    pub fn set_tx_attenuation(&mut self, mdb: u32) -> Result<(), Error> {
        ck("set_tx_attenuation", unsafe { ad9361_set_tx_attenuation(self.phy, 0, mdb) })
    }

    /// The stock firmware could leave this off with the DAC streaming happily
    /// into nothing; make it an explicit, checked operation.
    pub fn tx_lo_power(&mut self, on: bool) -> Result<(), Error> {
        ck("tx_lo_powerdown", unsafe { ad9361_tx_lo_powerdown(self.phy, if on { 0 } else { 1 }) })
    }

    pub fn set_rx_manual_gain(&mut self, db: i32) -> Result<(), Error> {
        unsafe {
            ck("set_rx_gain_control_mode", ad9361_set_rx_gain_control_mode(self.phy, 0, 0))?;
            ck("set_rx_rf_gain", ad9361_set_rx_rf_gain(self.phy, 0, db))
        }
    }

    /// Received signal strength in dB (relative; larger is stronger).
    pub fn rx_rssi_db(&mut self) -> Result<f32, Error> {
        let mut r = rf_rssi::default();
        ck("get_rx_rssi", unsafe { ad9361_get_rx_rssi(self.phy, 0, &mut r) })?;
        Ok(-(r.symbol as f32) / r.multiplier as f32)
    }

    pub fn temperature_c(&mut self) -> Result<f32, Error> {
        let mut t = 0;
        ck("get_temperature", unsafe { ad9361_get_temperature(self.phy, &mut t) })?;
        Ok(t as f32 / 1000.0)
    }

    /// Drive the DAC from the core's built-in DDS: one tone at `offset_hz`
    /// from the LO, `scale` of full scale (0..1).  No DMA involved, so this
    /// tests radio + interface + DAC core and nothing else.
    pub fn dds_tone(&mut self, offset_hz: u32, scale: f32) -> Result<(), Error> {
        let micro = (scale.clamp(0.0, 1.0) * 1_000_000.0) as i32;
        unsafe {
            let dac = (*self.phy).tx_dac;
            // DDS index = dac_channel * 2 + tone.  TX1 is channels 0 (I) and
            // 1 (Q); I leads Q by 90 degrees for a tone above the LO.  Second
            // tones are silenced.
            for idx in 0..4u32 {
                let (chan, tone) = (idx / 2, idx % 2);
                let on = tone == 0;
                ck("dds_set_frequency", axi_dac_dds_set_frequency(dac, idx, offset_hz))?;
                ck("dds_set_phase", axi_dac_dds_set_phase(dac, idx, if chan == 0 { 90_000 } else { 0 }))?;
                ck("dds_set_scale", axi_dac_dds_set_scale(dac, idx, if on { micro } else { 0 }))?;
            }
            ck("dac_set_datasel", axi_dac_set_datasel(dac, -1, axi_dac_data_sel_AXI_DAC_DATA_SEL_DDS))
        }
    }

    /// Route the DMA stream (rather than the DDS) to the DAC.
    pub fn dac_from_dma(&mut self) -> Result<(), Error> {
        unsafe { ck("dac_set_datasel", axi_dac_set_datasel((*self.phy).tx_dac, -1, axi_dac_data_sel_AXI_DAC_DATA_SEL_DMA)) }
    }
}
