/*
    Copyright (C) 2025  John Melton G0ORX/N6LYT

    This program is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    This program is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with this program.  If not, see <https://www.gnu.org/licenses/>.
*/

#![allow(non_camel_case_types)]
// Constants mirror WDSP's own C enum names verbatim (e.g.
// txaMeterType_TXA_CFC_GAIN) -- same "keep 1:1 with the C headers for
// easy cross-referencing" reasoning as non_camel_case_types above,
// rather than renaming ~26 constants (and their call sites) for no
// benefit besides a quieter build.
#![allow(non_upper_case_globals)]
// This binds WDSP's full C API surface; this app only calls a subset
// of it today, and more may get wired up as features are added --
// unused bindings aren't dead code to delete, they're the rest of the
// library sitting there for when it's needed.
#![allow(dead_code)]

/// Serializes one-time WDSP channel setup (OpenChannel and everything
/// around it, including the FFTW wisdom-generation pass triggered on a
/// fresh machine/config -- see spectrum.rs's WDSPwisdom call) across
/// EVERY channel this process opens, RXA and TXA alike -- not just
/// among receivers. FFTW's planner is well-documented as unsafe to
/// call concurrently from multiple threads, and every WDSP channel
/// (spectrum.rs's SpectrumAnalyzer::open for RXA, tx.rs's
/// TxProcessor::open for TXA) does exactly that during its one-time
/// setup, each on its own independent background thread.
///
/// Previously this was a `Mutex` local to SpectrumAnalyzer::open,
/// added after a segfault reproduced when multiple *receiver* threads
/// opened concurrently at startup -- it correctly serialized RXA
/// against RXA, but TXA's OpenChannel call in tx.rs took no lock at
/// all, so it could still race against an in-progress RXA setup (most
/// dangerously the wisdom pass, which can run long enough on a first-
/// ever launch for TX setup -- started unconditionally alongside RX at
/// initial connect, see main.rs's MicInput::start/TxHandle::start call
/// site -- to land right in the middle of it), corrupting FFTW's
/// shared planner state (glibc's "double free or corruption (!prev)"
/// is the classic symptom). Now shared here so both call sites
/// serialize against each other, not just against their own kind.
/// Each channel's own steady-state feed()/demod()/run() loop
/// afterward is untouched by this lock and continues to run fully
/// concurrently -- only the one-time setup sequence takes it.
pub static SETUP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub const DETECTOR_MODE_PEAK: u32 = 0;
pub const DETECTOR_MODE_ROSENFELL: u32 = 1;
pub const DETECTOR_MODE_AVERAGE: u32 = 2;
pub const DETECTOR_MODE_SAMPLE: u32 = 3;
pub const AVERAGE_MODE_NONE: u32 = 0;
pub const AVERAGE_MODE_RECURSIVE: u32 = 1;
pub const AVERAGE_MODE_TIME_WINDOW: u32 = 2;
pub const AVERAGE_MODE_LOG_RECURSIVE: u32 = 3;
unsafe extern "C" {
    pub fn GetWDSPVersion() -> ::std::os::raw::c_int;
}
unsafe extern "C" {
    pub fn OpenChannel(
        channel: ::std::os::raw::c_int,
        in_size: ::std::os::raw::c_int,
        dsp_size: ::std::os::raw::c_int,
        input_samplerate: ::std::os::raw::c_int,
        dsp_rate: ::std::os::raw::c_int,
        output_samplerate: ::std::os::raw::c_int,
        type_: ::std::os::raw::c_int,
        state: ::std::os::raw::c_int,
        tdelayup: f64,
        tslewup: f64,
        tdelaydown: f64,
        tslewdown: f64,
        bfo: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn CloseChannel(channel: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetType(channel: ::std::os::raw::c_int, type_: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetInputBuffsize(channel: ::std::os::raw::c_int, in_size: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetDSPBuffsize(channel: ::std::os::raw::c_int, dsp_size: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetInputSamplerate(channel: ::std::os::raw::c_int, samplerate: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetDSPSamplerate(channel: ::std::os::raw::c_int, samplerate: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetOutputSamplerate(channel: ::std::os::raw::c_int, samplerate: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetAllRates(
        channel: ::std::os::raw::c_int,
        in_rate: ::std::os::raw::c_int,
        dsp_rate: ::std::os::raw::c_int,
        out_rate: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetChannelState(
        channel: ::std::os::raw::c_int,
        state: ::std::os::raw::c_int,
        dmode: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int;
}
unsafe extern "C" {
    pub fn SetChannelTDelayUp(channel: ::std::os::raw::c_int, time: f64);
}
unsafe extern "C" {
    pub fn SetChannelTSlewUp(channel: ::std::os::raw::c_int, time: f64);
}
unsafe extern "C" {
    pub fn SetChannelTDelayDown(channel: ::std::os::raw::c_int, time: f64);
}
unsafe extern "C" {
    pub fn SetChannelTSlewDown(channel: ::std::os::raw::c_int, time: f64);
}
unsafe extern "C" {
    pub fn fexchange0(
        channel: ::std::os::raw::c_int,
        in_: *mut f64,
        out: *mut f64,
        error: *mut ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn fexchange2(
        channel: ::std::os::raw::c_int,
        Iin: *mut f32,
        Qin: *mut f32,
        Iout: *mut f32,
        Qout: *mut f32,
        error: *mut ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn XCreateAnalyzer(
        disp: ::std::os::raw::c_int,
        success: *mut ::std::os::raw::c_int,
        m_size: ::std::os::raw::c_int,
        m_num_fft: ::std::os::raw::c_int,
        m_stitch: ::std::os::raw::c_int,
        app_data_path: *mut ::std::os::raw::c_char,
    );
}
unsafe extern "C" {
    pub fn SetAnalyzer(
        disp: ::std::os::raw::c_int,
        n_pixout: ::std::os::raw::c_int,
        n_fft: ::std::os::raw::c_int,
        typ: ::std::os::raw::c_int,
        flp: *mut ::std::os::raw::c_int,
        sz: ::std::os::raw::c_int,
        bf_sz: ::std::os::raw::c_int,
        win_type: ::std::os::raw::c_int,
        pi: f64,
        ovrlp: ::std::os::raw::c_int,
        clp: ::std::os::raw::c_int,
        fscLin: f64,
        fscHin: f64,
        n_pix: ::std::os::raw::c_int,
        n_stch: ::std::os::raw::c_int,
        calset: ::std::os::raw::c_int,
        fmin: f64,
        fmax: f64,
        max_w: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn Spectrum0(
        run: ::std::os::raw::c_int,
        disp: ::std::os::raw::c_int,
        ss: ::std::os::raw::c_int,
        LO: ::std::os::raw::c_int,
        in_: *mut f64,
    );
}
unsafe extern "C" {
    pub fn Spectrum(
        disp: ::std::os::raw::c_int,
        ss: ::std::os::raw::c_int,
        LO: ::std::os::raw::c_int,
        pI: *mut f32,
        pQ: *mut f32,
    );
}
unsafe extern "C" {
    pub fn GetPixels(
        disp: ::std::os::raw::c_int,
        pixout: ::std::os::raw::c_int,
        pix: *mut f32,
        flag: *mut ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetDisplayDetectorMode(
        disp: ::std::os::raw::c_int,
        pixout: ::std::os::raw::c_int,
        mode: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetDisplayAverageMode(
        disp: ::std::os::raw::c_int,
        pixout: ::std::os::raw::c_int,
        mode: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetDisplayNumAverage(
        disp: ::std::os::raw::c_int,
        pixout: ::std::os::raw::c_int,
        num: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetDisplayAvBackmult(
        disp: ::std::os::raw::c_int,
        pixout: ::std::os::raw::c_int,
        mult: f64,
    );
}
unsafe extern "C" {
    pub fn SetDisplayNormOneHz(
        disp: ::std::os::raw::c_int,
        pixout: ::std::os::raw::c_int,
        norm: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetDisplaySampleRate(
        disp: ::std::os::raw::c_int,
        rate: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn DestroyAnalyzer(disp: ::std::os::raw::c_int);
}
pub const rxaMeterType_RXA_S_PK: rxaMeterType = 0;
pub const rxaMeterType_RXA_S_AV: rxaMeterType = 1;
pub const rxaMeterType_RXA_ADC_PK: rxaMeterType = 2;
pub const rxaMeterType_RXA_ADC_AV: rxaMeterType = 3;
pub const rxaMeterType_RXA_AGC_GAIN: rxaMeterType = 4;
pub const rxaMeterType_RXA_AGC_PK: rxaMeterType = 5;
pub const rxaMeterType_RXA_AGC_AV: rxaMeterType = 6;
pub const rxaMeterType_RXA_METERTYPE_LAST: rxaMeterType = 7;
pub type rxaMeterType = ::std::os::raw::c_uint;
unsafe extern "C" {
    pub fn SetRXAMode(channel: ::std::os::raw::c_int, mode: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXABandpassRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXABandpassFreqs(channel: ::std::os::raw::c_int, low: f64, high: f64);
}
unsafe extern "C" {
    pub fn RXASetPassband(channel: ::std::os::raw::c_int, f_low: f64, f_high: f64);
}
unsafe extern "C" {
    pub fn SetRXAFMSQRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAFMSQThreshold(channel: ::std::os::raw::c_int, threshold: f64);
}
unsafe extern "C" {
    pub fn SetRXAAMSQRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAAMSQThreshold(channel: ::std::os::raw::c_int, threshold: f64);
}
unsafe extern "C" {
    pub fn SetRXAEMNRRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXARNNRRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAEMNRgainMethod(channel: ::std::os::raw::c_int, method: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAEMNRnpeMethod(channel: ::std::os::raw::c_int, method: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAEMNRPosition(channel: ::std::os::raw::c_int, position: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAANRPosition(channel: ::std::os::raw::c_int, position: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXARNNRPosition(channel: ::std::os::raw::c_int, position: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAANFPosition(channel: ::std::os::raw::c_int, position: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAANFRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXASBNRRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXASBNRPosition(channel: ::std::os::raw::c_int, position: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXASBNRreductionAmount(channel: ::std::os::raw::c_int, nr4_reduction_amount: f32);
}
unsafe extern "C" {
    pub fn SetRXASBNRsmoothingFactor(channel: ::std::os::raw::c_int, nr4_smooting_factor: f32);
}
unsafe extern "C" {
    pub fn SetRXASBNRwhiteningFactor(channel: ::std::os::raw::c_int, nr4_whitening_factor: f32);
}
unsafe extern "C" {
    pub fn SetRXASBNRpostFilterThreshold(channel: ::std::os::raw::c_int, nr4_post_filter_threshold: f32);
}
unsafe extern "C" {
    pub fn SetRXASBNRnoiseRescale (channel: ::std::os::raw::c_int, nr4_noise_rescale: f32);
}
unsafe extern "C" {
    pub fn SetRXASBNRnoiseScalingType(channel: ::std::os::raw::c_int, noise_scaling_type: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn GetRXAMeter(channel: ::std::os::raw::c_int, mt: ::std::os::raw::c_int) -> f64;
}
unsafe extern "C" {
    pub fn SetRXAPanelBinaural(channel: ::std::os::raw::c_int, bin: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAPanelPan(channel: ::std::os::raw::c_int, pan: f64);
}
unsafe extern "C" {
    pub fn RXANBPSetFreqs(channel: ::std::os::raw::c_int, low: f64, high: f64);
}
unsafe extern "C" {
    pub fn RXANBPAddNotch(channel: ::std::os::raw::c_int, notch: ::std::os::raw::c_int, fcenter: f64, fwidth: f64, active: ::std::os::raw::c_int) -> ::std::os::raw::c_int;
}
unsafe extern "C" {
    pub fn RXANBPSetNotchesRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn RXANBPSetTuneFrequency(channel: ::std::os::raw::c_int, tunefreq: f64);
}
unsafe extern "C" {
    pub fn RXANBPSetShiftFrequency(channel: ::std::os::raw::c_int, shift: f64);
}
unsafe extern "C" {
    pub fn SetRXASNBAOutputBandwidth(channel: ::std::os::raw::c_int, low: f64, high: f64);
}
unsafe extern "C" {
    pub fn SetRXAANRRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAEMNRaeRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXASNBARun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAShiftRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAShiftFreq(channel: ::std::os::raw::c_int, fshift: f64);
}
unsafe extern "C" {
    pub fn SetRXAAMDSBMode(channel: ::std::os::raw::c_int, sbmode: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAANRVals(
        channel: ::std::os::raw::c_int,
        taps: ::std::os::raw::c_int,
        delay: ::std::os::raw::c_int,
        gain: f64,
        leakage: f64,
    );
}
unsafe extern "C" {
    pub fn SetRXAANFVals(
        channel: ::std::os::raw::c_int,
        taps: ::std::os::raw::c_int,
        delay: ::std::os::raw::c_int,
        gain: f64,
        leakage: f64,
    );
}
unsafe extern "C" {
    pub fn SetRXAAGCMode(channel: ::std::os::raw::c_int, mode: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAAGCFixed(channel: ::std::os::raw::c_int, fixed_agc: f64);
}
unsafe extern "C" {
    pub fn SetRXAAGCAttack(channel: ::std::os::raw::c_int, attack: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAAGCDecay(channel: ::std::os::raw::c_int, decay: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAAGCHang(channel: ::std::os::raw::c_int, hang: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn GetRXAAGCHangLevel(channel: ::std::os::raw::c_int, hangLevel: *mut f64);
}
unsafe extern "C" {
    pub fn SetRXAAGCHangLevel(channel: ::std::os::raw::c_int, hangLevel: f64);
}
unsafe extern "C" {
    pub fn GetRXAAGCHangThreshold(
        channel: ::std::os::raw::c_int,
        hangthreshold: *mut ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetRXAAGCHangThreshold(
        channel: ::std::os::raw::c_int,
        hangthreshold: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn GetRXAAGCTop(channel: ::std::os::raw::c_int, max_agc: *mut f64);
}
unsafe extern "C" {
    pub fn SetRXAAGCTop(channel: ::std::os::raw::c_int, max_agc: f64);
}
unsafe extern "C" {
    pub fn SetRXAAGCSlope(channel: ::std::os::raw::c_int, slope: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAAGCThresh(channel: ::std::os::raw::c_int, thresh: f64, size: f64, rate: f64);
}
unsafe extern "C" {
    pub fn GetRXAAGCThresh(channel: ::std::os::raw::c_int, thresh: *mut f64, size: f64, rate: f64);
}
unsafe extern "C" {
    pub fn SetRXAFMDeviation(channel: ::std::os::raw::c_int, deviation: f64);
}
unsafe extern "C" {
    pub fn RXASetNC(channel: ::std::os::raw::c_int, nc: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn RXASetMP(channel: ::std::os::raw::c_int, nc: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAEQRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAGrphEQ(channel: ::std::os::raw::c_int, rxeq: *mut ::std::os::raw::c_int);
}
unsafe extern "C" {
    // Not declared in the vendored wdsp.h this file was originally
    // generated from (only the 3-band SetRXAGrphEQ above is) -- but
    // confirmed present as a real exported symbol in vendor/libwdsp.a
    // via `nm` (eq.c defines it unconditionally, just without a header
    // prototype). Same array layout as SetRXAGrphEQ, just 11 entries
    // (preamp + 10 bands, 32Hz..16kHz) instead of 4.
    pub fn SetRXAGrphEQ10(channel: ::std::os::raw::c_int, rxeq: *mut ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn create_divEXT(
        id: ::std::os::raw::c_int,
        run: ::std::os::raw::c_int,
        nr: ::std::os::raw::c_int,
        size: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetEXTDIVRun(id: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTDIVBuffsize(id: ::std::os::raw::c_int, size: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTDIVNr(id: ::std::os::raw::c_int, nr: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTDIVOutput(id: ::std::os::raw::c_int, output: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTDIVRotate(
        id: ::std::os::raw::c_int,
        nr: ::std::os::raw::c_int,
        Irotate: *mut f64,
        Qrotate: *mut f64,
    );
}
unsafe extern "C" {
    pub fn xdivEXT(
        id: ::std::os::raw::c_int,
        nsamples: ::std::os::raw::c_int,
        in_: *mut *mut f64,
        out: *mut f64,
    );
}
unsafe extern "C" {
    pub fn create_anbEXT(
        id: ::std::os::raw::c_int,
        run: ::std::os::raw::c_int,
        buffsize: ::std::os::raw::c_int,
        samplerate: f64,
        tau: f64,
        hangtime: f64,
        advtime: f64,
        backtau: f64,
        threshold: f64,
    );
}
unsafe extern "C" {
    pub fn destroy_anbEXT(id: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn flush_anbEXT(id: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn xanbEXT(id: ::std::os::raw::c_int, in_: *mut f64, out: *mut f64);
}
unsafe extern "C" {
    pub fn SetEXTANBRun(id: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTANBSamplerate(id: ::std::os::raw::c_int, rate: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTANBTau(id: ::std::os::raw::c_int, tau: f64);
}
unsafe extern "C" {
    pub fn SetEXTANBHangtime(id: ::std::os::raw::c_int, time: f64);
}
unsafe extern "C" {
    pub fn SetEXTANBAdvtime(id: ::std::os::raw::c_int, time: f64);
}
unsafe extern "C" {
    pub fn SetEXTANBBacktau(id: ::std::os::raw::c_int, tau: f64);
}
unsafe extern "C" {
    pub fn SetEXTANBThreshold(id: ::std::os::raw::c_int, thresh: f64);
}
unsafe extern "C" {
    pub fn xanbEXTF(id: ::std::os::raw::c_int, I: *mut f32, Q: *mut f32);
}
unsafe extern "C" {
    pub fn create_nobEXT(
        id: ::std::os::raw::c_int,
        run: ::std::os::raw::c_int,
        mode: ::std::os::raw::c_int,
        buffsize: ::std::os::raw::c_int,
        samplerate: f64,
        slewtime: f64,
        hangtime: f64,
        advtime: f64,
        backtau: f64,
        threshold: f64,
    );
}
unsafe extern "C" {
    pub fn destroy_nobEXT(id: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn flush_nobEXT(id: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn xnobEXT(id: ::std::os::raw::c_int, in_: *mut f64, out: *mut f64);
}
unsafe extern "C" {
    pub fn SetEXTNOBRun(id: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTNOBMode(id: ::std::os::raw::c_int, mode: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTNOBBuffsize(id: ::std::os::raw::c_int, size: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTNOBSamplerate(id: ::std::os::raw::c_int, rate: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEXTNOBTau(id: ::std::os::raw::c_int, tau: f64);
}
unsafe extern "C" {
    pub fn SetEXTNOBHangtime(id: ::std::os::raw::c_int, time: f64);
}
unsafe extern "C" {
    pub fn SetEXTNOBAdvtime(id: ::std::os::raw::c_int, time: f64);
}
unsafe extern "C" {
    pub fn SetEXTNOBBacktau(id: ::std::os::raw::c_int, tau: f64);
}
unsafe extern "C" {
    pub fn SetEXTNOBThreshold(id: ::std::os::raw::c_int, thresh: f64);
}
unsafe extern "C" {
    pub fn xnobEXTF(id: ::std::os::raw::c_int, I: *mut f32, Q: *mut f32);
}
pub const txaMeterType_TXA_MIC_PK: txaMeterType = 0;
pub const txaMeterType_TXA_MIC_AV: txaMeterType = 1;
pub const txaMeterType_TXA_EQ_PK: txaMeterType = 2;
pub const txaMeterType_TXA_EQ_AV: txaMeterType = 3;
pub const txaMeterType_TXA_LVLR_PK: txaMeterType = 4;
pub const txaMeterType_TXA_LVLR_AV: txaMeterType = 5;
pub const txaMeterType_TXA_LVLR_GAIN: txaMeterType = 6;
pub const txaMeterType_TXA_CFC_PK: txaMeterType = 7;
pub const txaMeterType_TXA_CFC_AV: txaMeterType = 8;
pub const txaMeterType_TXA_CFC_GAIN: txaMeterType = 9;
pub const txaMeterType_TXA_COMP_PK: txaMeterType = 10;
pub const txaMeterType_TXA_COMP_AV: txaMeterType = 11;
pub const txaMeterType_TXA_ALC_PK: txaMeterType = 12;
pub const txaMeterType_TXA_ALC_AV: txaMeterType = 13;
pub const txaMeterType_TXA_ALC_GAIN: txaMeterType = 14;
pub const txaMeterType_TXA_OUT_PK: txaMeterType = 15;
pub const txaMeterType_TXA_OUT_AV: txaMeterType = 16;
pub const txaMeterType_TXA_METERTYPE_LAST: txaMeterType = 17;
pub type txaMeterType = ::std::os::raw::c_uint;
unsafe extern "C" {
    pub fn SetTXAMode(channel: ::std::os::raw::c_int, mode: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXABandpassRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXABandpassFreqs(channel: ::std::os::raw::c_int, low: f64, high: f64);
}
unsafe extern "C" {
    pub fn SetTXABandpassWindow(channel: ::std::os::raw::c_int, wintype: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAEQRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXACTCSSRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAAMSQRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXACompressorGain(channel: ::std::os::raw::c_int, gain: f64);
}
unsafe extern "C" {
    pub fn SetTXACompressorRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAosctrlRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXACFIRRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn GetTXAMeter(channel: ::std::os::raw::c_int, mt: ::std::os::raw::c_int) -> f64;
}
unsafe extern "C" {
    pub fn SetTXAALCSt(channel: ::std::os::raw::c_int, state: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAALCAttack(channel: ::std::os::raw::c_int, attack: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAALCDecay(channel: ::std::os::raw::c_int, decay: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAALCHang(channel: ::std::os::raw::c_int, hang: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAALCMaxGain(channel: ::std::os::raw::c_int, maxgain: f64);
}
unsafe extern "C" {
    pub fn SetTXALevelerSt(channel: ::std::os::raw::c_int, state: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXALevelerAttack(channel: ::std::os::raw::c_int, attack: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXALevelerDecay(channel: ::std::os::raw::c_int, decay: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXALevelerHang(channel: ::std::os::raw::c_int, hang: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXALevelerTop(channel: ::std::os::raw::c_int, maxgain: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAPreGenMode(channel: ::std::os::raw::c_int, mode: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAPreGenToneMag(channel: ::std::os::raw::c_int, mag: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenToneFreq(channel: ::std::os::raw::c_int, freq: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenNoiseMag(channel: ::std::os::raw::c_int, mag: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenSweepMag(channel: ::std::os::raw::c_int, mag: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenSweepFreq(channel: ::std::os::raw::c_int, freq1: f64, freq2: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenSweepRate(channel: ::std::os::raw::c_int, rate: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenSawtoothMag(channel: ::std::os::raw::c_int, mag: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenSawtoothFreq(channel: ::std::os::raw::c_int, freq: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenTriangleMag(channel: ::std::os::raw::c_int, mag: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenTriangleFreq(channel: ::std::os::raw::c_int, freq: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenPulseMag(channel: ::std::os::raw::c_int, mag: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenPulseFreq(channel: ::std::os::raw::c_int, freq: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenPulseDutyCycle(channel: ::std::os::raw::c_int, dc: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenPulseToneFreq(channel: ::std::os::raw::c_int, freq: f64);
}
unsafe extern "C" {
    pub fn SetTXAPreGenPulseTransition(channel: ::std::os::raw::c_int, transtime: f64);
}
unsafe extern "C" {
    pub fn SetTXAPostGenRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAPostGenMode(channel: ::std::os::raw::c_int, mode: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAPostGenToneMag(channel: ::std::os::raw::c_int, mag: f64);
}
unsafe extern "C" {
    pub fn SetTXAPostGenToneFreq(channel: ::std::os::raw::c_int, freq: f64);
}
unsafe extern "C" {
    pub fn SetTXAPostGenTTMag(channel: ::std::os::raw::c_int, mag1: f64, mag2: f64);
}
unsafe extern "C" {
    pub fn SetTXAPostGenTTFreq(channel: ::std::os::raw::c_int, freq1: f64, freq2: f64);
}
unsafe extern "C" {
    pub fn SetTXAPostGenSweepMag(channel: ::std::os::raw::c_int, mag: f64);
}
unsafe extern "C" {
    pub fn SetTXAPostGenSweepFreq(channel: ::std::os::raw::c_int, freq1: f64, freq2: f64);
}
unsafe extern "C" {
    pub fn SetTXAPostGenSweepRate(channel: ::std::os::raw::c_int, rate: f64);
}
unsafe extern "C" {
    pub fn SetTXAGrphEQ(channel: ::std::os::raw::c_int, txeq: *mut ::std::os::raw::c_int);
}
unsafe extern "C" {
    // See SetRXAGrphEQ10's doc comment -- same reasoning, TXA side.
    pub fn SetTXAGrphEQ10(channel: ::std::os::raw::c_int, txeq: *mut ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAFMDeviation(channel: ::std::os::raw::c_int, deviation: f64);
}
unsafe extern "C" {
    pub fn SetTXAFMEmphPosition(channel: ::std::os::raw::c_int, position: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn TXASetNC(channel: ::std::os::raw::c_int, nc: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn TXASetMP(channel: ::std::os::raw::c_int, nc: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAAMCarrierLevel(channel: ::std::os::raw::c_int, c_level: f64);
}
unsafe extern "C" {
    pub fn SetPSRunCal(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSMox(channel: ::std::os::raw::c_int, mox: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSReset(channel: ::std::os::raw::c_int, reset: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSMancal(channel: ::std::os::raw::c_int, mancal: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSAutomode(channel: ::std::os::raw::c_int, automode: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSTurnon(channel: ::std::os::raw::c_int, turnon: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSControl(
        channel: ::std::os::raw::c_int,
        reset: ::std::os::raw::c_int,
        mancal: ::std::os::raw::c_int,
        automode: ::std::os::raw::c_int,
        turnon: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetPSLoopDelay(channel: ::std::os::raw::c_int, delay: f64);
}
unsafe extern "C" {
    pub fn SetPSMoxDelay(channel: ::std::os::raw::c_int, delay: f64);
}
unsafe extern "C" {
    pub fn SetPSTXDelay(channel: ::std::os::raw::c_int, delay: f64) -> f64;
}
unsafe extern "C" {
    pub fn SetPSHWPeak(channel: ::std::os::raw::c_int, peak: f64);
}
unsafe extern "C" {
    pub fn SetPSPtol(channel: ::std::os::raw::c_int, ptol: f64);
}
unsafe extern "C" {
    /// Diagnostic-only -- exposes calcc's internal scatter data
    /// (x/ym/yc/ys, nsamps=ints*spi=4096 doubles each) and fitted
    /// correction-table coefficients (cm/cc/cs, ints*4=64 doubles
    /// each). Caller must size buffers to match TXA.c's fixed
    /// create_calcc call (ints=16, spi=256) -- there is no getter for
    /// ints/spi themselves, they're a compile-time constant on the
    /// WDSP side for this project's usage.
    pub fn GetPSDisp(
        channel: ::std::os::raw::c_int,
        x: *mut f64,
        ym: *mut f64,
        yc: *mut f64,
        ys: *mut f64,
        cm: *mut f64,
        cc: *mut f64,
        cs: *mut f64,
    );
}
unsafe extern "C" {
    pub fn SetPSFeedbackRate(channel: ::std::os::raw::c_int, rate: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSIntsAndSpi(
        channel: ::std::os::raw::c_int,
        ints: ::std::os::raw::c_int,
        spi: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetPSStabilize(channel: ::std::os::raw::c_int, stbl: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSMapMode(channel: ::std::os::raw::c_int, map: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetPSPinMode(channel: ::std::os::raw::c_int, pin: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn GetPSInfo(channel: ::std::os::raw::c_int, info: *mut ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn GetPSHWPeak(channel: ::std::os::raw::c_int, peak: *mut f64);
}
unsafe extern "C" {
    pub fn GetPSMaxTX(channel: ::std::os::raw::c_int, maxtx: *mut f64);
}
unsafe extern "C" {
    /// Queues an async save of the current correction table to
    /// `filename` (confirmed via calcc.c: this just signals a
    /// background WDSP-internal thread via a semaphore and returns
    /// immediately, not a blocking file write on the calling thread).
    pub fn PSSaveCorr(channel: ::std::os::raw::c_int, filename: *mut ::std::os::raw::c_char);
}
unsafe extern "C" {
    /// Queues an async load of a previously-saved correction table from
    /// `filename` and applies it -- confirmed via calcc.c: this also
    /// sets WDSP's internal `turnon` flag, which puts the engine into a
    /// one-shot "apply this table, don't (re)calibrate" mode (distinct
    /// from continuous auto-calibrate) and clears `automode` once
    /// applied, so continuous auto-calibrate needs re-enabling
    /// separately afterward if wanted.
    pub fn PSRestoreCorr(channel: ::std::os::raw::c_int, filename: *mut ::std::os::raw::c_char);
}
unsafe extern "C" {
    pub fn pscc(
        channel: ::std::os::raw::c_int,
        size: ::std::os::raw::c_int,
        tx: *mut f64,
        rx: *mut f64,
    );
}
unsafe extern "C" {
    /// Float-buffer variant of `pscc` above (wdsp/calcc.c) -- separate
    /// I/Q buffers rather than interleaved complex pairs, and f32
    /// rather than f64, matching this project's own convention
    /// elsewhere (avoids a f32->f64 conversion buffer on every call).
    /// `mox`/`solidmox` let WDSP know the current PTT state directly
    /// as part of the call, confirmed against piHPSDR's transmitter.c
    /// call site.
    pub fn psccF(
        channel: ::std::os::raw::c_int,
        size: ::std::os::raw::c_int,
        itxbuff: *mut f32,
        qtxbuff: *mut f32,
        irxbuff: *mut f32,
        qrxbuff: *mut f32,
        mox: ::std::os::raw::c_int,
        solidmox: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn create_eerEXT(
        id: ::std::os::raw::c_int,
        run: ::std::os::raw::c_int,
        size: ::std::os::raw::c_int,
        rate: ::std::os::raw::c_int,
        mgain: f64,
        pgain: f64,
        rundelays: ::std::os::raw::c_int,
        mdelay: f64,
        pdelay: f64,
        amiq: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn xeerEXTF(
        id: ::std::os::raw::c_int,
        inI: *mut f32,
        inQ: *mut f32,
        outI: *mut f32,
        outQ: *mut f32,
        outMI: *mut f32,
        outMQ: *mut f32,
        mox: ::std::os::raw::c_int,
        size: ::std::os::raw::c_int,
    );
}
unsafe extern "C" {
    pub fn SetEERRun(id: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEERAMIQ(id: ::std::os::raw::c_int, amiq: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEERRunDelays(id: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetEERPgain(id: ::std::os::raw::c_int, gain: f64);
}
unsafe extern "C" {
    pub fn SetEERPdelay(id: ::std::os::raw::c_int, delay: f64);
}
unsafe extern "C" {
    pub fn SetEERMgain(id: ::std::os::raw::c_int, gain: f64);
}
unsafe extern "C" {
    pub fn SetEERMdelay(id: ::std::os::raw::c_int, delay: f64);
}
unsafe extern "C" {
    pub fn create_resample(
        run: ::std::os::raw::c_int,
        size: ::std::os::raw::c_int,
        in_: *mut f64,
        out: *mut f64,
        in_rate: ::std::os::raw::c_int,
        out_rate: ::std::os::raw::c_int,
        fc: f64,
        ncoef: ::std::os::raw::c_int,
        gain: f64,
    ) -> *mut ::std::os::raw::c_void;
}
unsafe extern "C" {
    pub fn destroy_resample(a: *mut ::std::os::raw::c_void);
}
unsafe extern "C" {
    pub fn flush_resample(a: *mut ::std::os::raw::c_void);
}
unsafe extern "C" {
    pub fn xresample(a: *mut ::std::os::raw::c_void) -> ::std::os::raw::c_int;
}
unsafe extern "C" {
    pub fn create_resampleFV(
        in_rate: ::std::os::raw::c_int,
        out_rate: ::std::os::raw::c_int,
    ) -> *mut ::std::os::raw::c_void;
}
unsafe extern "C" {
    pub fn xresampleFV(
        input: *mut f32,
        output: *mut f32,
        numsamps: ::std::os::raw::c_int,
        outsamps: *mut ::std::os::raw::c_int,
        ptr: *mut ::std::os::raw::c_void,
    );
}
unsafe extern "C" {
    pub fn destroy_resampleFV(ptr: *mut ::std::os::raw::c_void);
}
unsafe extern "C" {
    pub fn SetRXAPanelRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAPanelSelect(channel: ::std::os::raw::c_int, select: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetRXAPanelGain1(channel: ::std::os::raw::c_int, gain: f64);
}
unsafe extern "C" {
    pub fn SetRXAPanelGain2(channel: ::std::os::raw::c_int, gainI: f64, gainQ: f64);
}
unsafe extern "C" {
    pub fn SetRXAPanelCopy(channel: ::std::os::raw::c_int, copy: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAPanelRun(channel: ::std::os::raw::c_int, run: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAPanelSelect(channel: ::std::os::raw::c_int, select: ::std::os::raw::c_int);
}
unsafe extern "C" {
    pub fn SetTXAPanelGain1(channel: ::std::os::raw::c_int, gain: f64);
}
unsafe extern "C" {
    pub fn create_varsampV(
        in_rate: ::std::os::raw::c_int,
        out_rate: ::std::os::raw::c_int,
        R: ::std::os::raw::c_int,
    ) -> *mut ::std::os::raw::c_void;
}
unsafe extern "C" {
    pub fn xvarsampV(
        input: *mut f64,
        output: *mut f64,
        numsamps: ::std::os::raw::c_int,
        var: f64,
        outsamps: *mut ::std::os::raw::c_int,
        ptr: *mut ::std::os::raw::c_void,
    );
}
unsafe extern "C" {
    pub fn destroy_varsampV(ptr: *mut ::std::os::raw::c_void);
}
unsafe extern "C" {
    pub fn SetTXACTCSSFreq(channel: ::std::os::raw::c_int, freq: f64);
}
unsafe extern "C" {
    pub fn wisdom_get_status() -> *mut ::std::os::raw::c_char;
}
unsafe extern "C" {
    pub fn WDSPwisdom(directory: *const ::std::os::raw::c_char);
}
