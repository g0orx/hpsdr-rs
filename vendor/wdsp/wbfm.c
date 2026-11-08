/*  wbfm.c

This file is part of a program that implements a Software-Defined Radio.

Copyright (C) 2026 Warren Pratt, NR0V

This program is free software; you can redistribute it and/or
modify it under the terms of the GNU General Public License
as published by the Free Software Foundation; either version 2
of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program; if not, write to the Free Software
Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.

The author can be reached by email at

warren@pratt.one
*/

#define _CRT_SECURE_NO_WARNINGS
#include <limits.h>
#include "comm.h"

typedef struct _wbfm
{
	// the basics
	int run;
	int size;
	double* in;
	double* out;
	double rate;

	// input filter:  +/- 128 kHz, ON at 384K and higher
	int run_filin;
	FIRCORE pfilin;
	double* filin_out;
	double flow_filin;
	double fhigh_filin;
	int nc_filin;

	// internal buffers
	double* disc_out;
	double* fil19_out;
	double* fil53_out;
	double* LmR;
	double* LpR;
	double* L;
	double* R;

	// discriminator & dcb
	double discSave_I;
	double discSave_Q;
	double dcb_fc;
	double dcb_R;
	double dcbSave_x[2];			// [0] for L, [1] for R
	double dcbSave_y[2];			// [0] for L, [1] for R
	double disc_gain_comp;
	
	// filter:  0-15 kHz
	FIRCORE pfil0_15;
	double flow_fil0_15;
	double fhigh_fil0_15;
	int nc_fil0_15;

	// filter:  19 kHz
	FIRCORE pfil19;
	double flow_fil19;
	double fhigh_fil19;
	double magComp_fil19;
	int nc_fil19;

	// filter:  23-53 kHz
	FIRCORE pfil23_53;
	double flow_fil23_53;
	double fhigh_fil23_53;
	double magComp_fil23_53;
	int nc_fil23_53;

	// Pilot AGC
	double tau_attack;
	double n_tau;
	double targ_mult;
	double out_targ;
	int agc_delay;
	WCPAGC pAGC_Pilot;

	// Deemphasis Filters
	DMPH dmphL;
	DMPH dmphR;

	// Indicator
	INDY pIndy;

	// Squelch
	WBSQL psql;

	// Miscellaneous
	int stereo;
	double sqgain;
	double mag19;
	int dmph;
	int dmph_type;

	// Matching Delays
	SDELAY del15;
	SDELAY del53;

} wbfm, *WBFM;

/********************************************************************************************************
*																										*
*                                     STEREO ACTIVATION & INDICATOR                                     *
*																										*
********************************************************************************************************/


typedef struct _indy
{
	int size;
	double rate;
	double* pilot;
	double phs_delta;
	double phs;
	double filt_cutoff;
	double alpha;
	double oneMinusAlpha;
	double filtIstable;
	double filtQstable;
	double low_thresh;
	double high_thresh;
	int on_delay_frames;
	int off_delay_frames;
	int consecutive_high_frames;
	int consecutive_low_frames;
	int stereo;
}indy, * INDY;

static INDY create_indy(int size, double rate, double* pilot)
{
	INDY a = (INDY)malloc0(sizeof(indy));
	a->size = size;
	a->rate = rate;
	a->pilot = pilot;
	a->phs_delta = TWOPI * 19000.0 / a->rate;
	a->phs = 0.0;
	a->filt_cutoff = 4.0;									// Tune this!
	a->alpha = (TWOPI * a->filt_cutoff) / a->rate;
	a->oneMinusAlpha = 1.0 - a->alpha;
	a->filtIstable = 0.0;
	a->filtQstable = 0.0;
	a->low_thresh  = 0.03;									// Tune this!
	a->high_thresh = 0.06;									// Tune this!
	a->on_delay_frames  = (int)((0.5 * a->rate)/ a->size);
	a->off_delay_frames = (int)((1.0 * a->rate)/ a->size);
	a->consecutive_high_frames = 0;
	a->consecutive_low_frames  = 0;
	a->stereo = 0;
	return a;
}

static void destroy_indy(INDY a)
{
	_aligned_free(a);
}

static void flush_indy(INDY a)
{
	a->phs = 0.0;
	a->filtIstable = 0.0;
	a->filtQstable = 0.0;
	a->consecutive_high_frames = 0;
	a->consecutive_low_frames = 0;
	a->stereo = 0;
}

static void xindy(INDY a, int* stereo, double* magnitude)
{
	double mag  = 0.0;
	double mixI = 0.0, mixQ = 0.0;
	double oscI = 0.0, oscQ = 0.0;
	for (int i = 0; i < a->size; i++)
	{
		oscI = +cos(a->phs);
		oscQ = +sin(a->phs);
		a->phs += a->phs_delta;
		if (a->phs >= TWOPI) a->phs -= TWOPI;
		mixI = a->pilot[2 * i + 0] * oscI - a->pilot[2 * i + 1] * oscQ;
		mixQ = a->pilot[2 * i + 0] * oscQ + a->pilot[2 * i + 1] * oscI;
		a->filtIstable = a->alpha * mixI + a->oneMinusAlpha * a->filtIstable;
		a->filtQstable = a->alpha * mixQ + a->oneMinusAlpha * a->filtQstable;
	}
	mag = sqrt(a->filtIstable * a->filtIstable + a->filtQstable * a->filtQstable);
	if (mag > a->high_thresh)
	{
		a->consecutive_low_frames = 0;
		if (a->consecutive_high_frames < INT_MAX) a->consecutive_high_frames++;
		if (a->consecutive_high_frames >= a->on_delay_frames)
			a->stereo = 1;
	}
	else if (mag < a->low_thresh)
	{
		a->consecutive_high_frames = 0;
		if (a->consecutive_low_frames < INT_MAX) a->consecutive_low_frames++;
		if (a->consecutive_low_frames >= a->off_delay_frames)
			a->stereo = 0;
	}
	else
	{
		a->consecutive_low_frames = 0;
		a->consecutive_high_frames = 0;
	}
	*stereo = a->stereo;
	*magnitude = mag;
	//printf("Pilot Mag = %.4e\n", mag);
}


/********************************************************************************************************
*																										*
*                                               DE-EMPHASIS                                             *
*																										*
********************************************************************************************************/

typedef struct _dmph
{
	int run;
	double rate;
	int size;
	double* in;
	double* out;
	double tau;				// de-emphasis time-constant
	double b[2];
	double a[2];
	double x_z1;
	double y_z1;
}dmph, * DMPH;

static void calc_dmph(DMPH a)
{
	double c = 1.0 / tan(1.0 / (2.0 * a->rate * a->tau));
	double denominator = 1.0 + c;
	a->b[0] = 1.0 / denominator;
	a->b[1] = 1.0 / denominator;
	a->a[0] = 1.0;
	a->a[1] = (1.0 - c) / denominator;
	a->x_z1 = 0.0;
	a->y_z1 = 0.0;
}

static DMPH create_dmph(int run, double rate, int dmph_type, int size, double* in, double* out)
{
	DMPH a = (DMPH)malloc0(sizeof(dmph));
	a->run = run;
	a->rate = rate;
	a->size = size;
	a->in = in;
	a->out = out;
	if (!dmph_type) a->tau = 75.0e-6;
	else            a->tau = 50.0e-6;
	calc_dmph(a);
	return a;
}

static void destroy_dmph(DMPH a)
{
	_aligned_free(a);
}

static void xdmph(DMPH a)
{
	if (a->run)
	{
		double x = 0.0, y = 0.0;
		for (int i = 0; i < a->size; i++)
		{
			x = a->in[i];
			y = (a->b[0] * x) + (a->b[1] * a->x_z1) - (a->a[1] * a->y_z1);
			a->x_z1 = x;
			a->y_z1 = y;
			a->out[i] = y;
		}
	}
	else if (a->out != a->in)
		memcpy(a->out, a->in, a->size * sizeof(double));
}

static void flush_dmph(DMPH a)
{
	a->x_z1 = 0.0;
	a->y_z1 = 0.0;
}

static void setValues_dmph(DMPH a, int run, int dmph_type)
{
	// [Added 2026-11.  SetRXAWBFMdmph() formerly stored the new run flag and type only in the
	//  WBFM struct, and the de-emphasis filters received them only when calc_wbfm() next ran
	//  (a sample rate or buffer size change).  Turning de-emphasis ON/OFF, or switching between
	//  75 and 50 usec, therefore did not take effect until then.  This applies them now.]
	a->run = run;
	if (!dmph_type) a->tau = 75.0e-6;
	else            a->tau = 50.0e-6;
	calc_dmph(a);
}


/************************************************************************************************
*																								*
*							                  SQUELCH          			    					*
*																								*
************************************************************************************************/

typedef struct _bq_section
{
	double b0, b1, b2;
	double a1, a2;
	double x_z1, x_z2;
	double y_z1, y_z2;
}bqs, *BQS;

typedef struct _noisepower
{
	double rate;
	int size;
	double* in;
	double* buff;
	double alpha_attack;
	double alpha_release;
	double pwr;
	bqs bqsec[2];
}nopwr, *NOPWR;

static void calc_bqs(double fs, double f_low, double f_high, bqs bqsec[])
{
	double w_low = 2.0 * fs * tan(PI * f_low / fs);
	double w_high = 2.0 * fs * tan(PI * f_high / fs);
	double BW = w_high - w_low;
	double W0 = sqrt(w_low * w_high);
	double pole_re[2] = { -sin(PI / 4.0), -sin(3.0 * PI / 4.0) };
	double pole_im[2] = {  cos(PI / 4.0),  cos(3.0 * PI / 4.0) };
	double W0sq = W0 * W0;
	double T = 1.0 / fs;
	for (int k = 0; k < 2; k++) 
	{
		double a1_a = -BW * pole_re[k];
		double a2_a = W0sq + BW * BW * (pole_im[k] * pole_im[k]
			- pole_re[k] * pole_re[k]);
		double K = 2.0 / T;
		double Ksq = K * K;
		double d0 = Ksq + a1_a * K + a2_a;
		double d1 = -2.0 * Ksq + 2.0 * a2_a;
		double d2 = Ksq - a1_a * K + a2_a;
		double n0 = BW * K;
		double n1 = 0.0;
		double n2 = -BW * K;
		bqsec[k].b0 = n0 / d0;
		bqsec[k].b1 = n1 / d0;
		bqsec[k].b2 = n2 / d0;
		bqsec[k].a1 = d1 / d0;   /* stored as positive; subtract in   */
		bqsec[k].a2 = d2 / d0;   /* the difference equation           */
	}
	for (int k = 0; k < 2; k++)
	{
		bqsec[k].x_z1 = 0.0;
		bqsec[k].x_z2 = 0.0;
		bqsec[k].y_z1 = 0.0;
		bqsec[k].y_z2 = 0.0;
	}

	/*printf("4th-order Butterworth Bandpass Filter Coefficients\n");
	printf("  fs       = %.1f Hz\n", fs);
	printf("  f_low    = %.1f Hz\n", f_low);
	printf("  f_high   = %.1f Hz\n", f_high);
	printf("\n");

	for (int k = 0; k < 2; k++) 
	{
		printf("Section %d:\n", k);
		printf("  b0 = % .10f\n", bqsec[k].b0);
		printf("  b1 = % .10f\n", bqsec[k].b1);
		printf("  b2 = % .10f\n", bqsec[k].b2);
		printf("  a1 = % .10f\n", bqsec[k].a1);
		printf("  a2 = % .10f\n", bqsec[k].a2);
		printf("\n");
	}*/

}

static void init_nopwr(double rate, int size, double* in, nopwr* npwr)
{
	npwr->rate = rate;
	npwr->size = size;
	npwr->in = in;
	npwr->buff = (double*)malloc0(npwr->size * sizeof(double));
	calc_bqs(npwr->rate, 58000.0, 75000.0, npwr->bqsec);
	npwr->alpha_attack  = exp(-1.0 / (rate * 0.050));	//  50 ms
	npwr->alpha_release = exp(-1.0 / (rate * 0.500));	// 500 ms
	npwr->pwr = 0.0;
}

static void bqsecRun(BQS a, int size, double* buff)
{
	double x = 0.0, y = 0.0;
	for (int i = 0; i < size; i++)
	{
		x = buff[i];
		y =   a->b0 * x
			+ a->b1 * a->x_z1
			+ a->b2 * a->x_z2
			- a->a1 * a->y_z1
			- a->a2 * a->y_z2;
		a->x_z2 = a->x_z1;
		a->x_z1 = x;
		a->y_z2 = a->y_z1;
		a->y_z1 = y;
		buff[i] = y;
	}
}

static double nopwrRun(NOPWR a)
{
	memcpy(a->buff, a->in, a->size * sizeof(double));
	for (int i = 0; i < 2; i++)
	{
		bqsecRun(&(a->bqsec[i]), a->size, a->buff);
	}
	for (int i = 0; i < a->size; i++)
	{
		double new_sample = a->buff[i] * a->buff[i];
		double alpha = (new_sample > a->pwr) ? a->alpha_attack : a->alpha_release;
		a->pwr = alpha * a->pwr + (1.0 - alpha) * new_sample;
	}
	return a->pwr;
}

typedef struct _wbsql
{
	double* in;
	double rate;
	int size;
	nopwr npwr;
	double mag19;			
	double pwrNoise;
	double pilot_true_thresh;
	double pilot_good_thresh;
	double noise_high_thresh;
	double noise_low_thresh;
	double slew;
	double gain_min;
	double gain;
	int sqstate;
} wbsql, *WBSQL;

static WBSQL create_wbsquelch(double* in, double rate, int size)
{
	WBSQL s = (WBSQL)malloc0(sizeof(wbsql));
	s->in = in;
	s->rate = rate;
	s->size = size;
	init_nopwr(s->rate, s->size, s->in, &s->npwr);
	s->mag19 = 0.0;
	s->pwrNoise = 0.0;
	s->pilot_true_thresh = 0.03;					// Tune this!
	s->pilot_good_thresh = 0.06;					// Tune this!
	s->noise_high_thresh = 0.50;					// Tune this!
	s->noise_low_thresh  = 0.15;					// Tune this!
	s->slew = (double)s->size / (0.05 * s->rate);	// 50ms
	s->gain_min = 0.050;							// Tune this!
	s->gain = s->gain_min;
	s->sqstate = 0;
	return s;
}

static void xwbsquelch(WBSQL s, double mag19, double* gain)
{
	s->mag19 = mag19;
	s->pwrNoise = nopwrRun(&(s->npwr));
	int noise_high = (s->pwrNoise > s->noise_high_thresh);
	int noise_low  = (s->pwrNoise < s->noise_low_thresh);
	int pilot_present = (s->mag19 > s->pilot_true_thresh);
	int pilot_good    = (s->mag19 > s->pilot_good_thresh);
	double gain_target = 0.0;
	switch (s->sqstate)
	{
	case 0:
		if (noise_low && (pilot_good || !pilot_present))
		{
			s->sqstate = 1;
		}
		break;
	case 1:
		if (noise_high || (pilot_present && !pilot_good))
		{
			s->sqstate = 0;
		}
		break;
	default:
		break;
	}
	if (s->sqstate) gain_target = 1.0;
	else            gain_target = s->gain_min;
	// [Slew toward the target and stop AT the target; fixed 2026-11.  Formerly, with the squelch
	//  open, the gain reached 1.0, the 'else' branch then stepped it DOWN by 'slew', and the next
	//  frame stepped it back up.  The open-squelch gain therefore alternated between 1.0 and
	//  (1.0 - slew) every frame, amplitude modulating the audio at rate / (2 * size).  At 192K,
	//  that put sidebands around every audio component at:
	//      size   256:  -41 dB, +/- 375 Hz         size  2048:  -22 dB, +/-  47 Hz
	//      size   512:  -35 dB, +/- 188 Hz         size  4096:  -15 dB, +/-  23 Hz
	//      size  1024:  -29 dB, +/-  94 Hz
	//  (At 384K the same 'size' gives half the 'slew' and about 6 dB lower sidebands.)]
	if (s->gain < gain_target)
		s->gain = fmin(s->gain + s->slew, gain_target);
	else if (s->gain > gain_target)
		s->gain = fmax(s->gain - s->slew, gain_target);
	*gain = s->gain;
}

static void destroy_wbsquelch(WBSQL s)
{
	_aligned_free(s->npwr.buff);
	_aligned_free(s);
}

/************************************************************************************************
*																								*
*							                DELAY           									*
*																								*
************************************************************************************************/

typedef struct _sdelay
{
	uint32_t size;
	uint32_t inpos;
	uint32_t outpos;
	uint32_t samp_delay;
	double* in;
	double* out;
	double* buff;
	uint32_t bsize;
}sdelay, * SDELAY;

static void calc_delay(SDELAY a)
{
	a->outpos = 0;
	a->inpos = a->outpos + a->samp_delay;
	a->buff = (double*)malloc0(a->size * sizeof(complex));
}

// for 'double', not 'complex' data
static SDELAY create_sdelay(uint32_t dl_size, uint32_t s_delay, uint32_t buff_size, double* in, double* out)
{
	SDELAY a = (SDELAY)malloc0(sizeof(sdelay));
	a->samp_delay = s_delay;
	a->size = dl_size;
	a->bsize = buff_size;
	a->in = in;
	a->out = out;
	calc_delay(a);
	return a;
}

static void decalc_delay(SDELAY a)
{
	_aligned_free(a->buff);
}

static void destroy_sdelay(SDELAY a)
{
	decalc_delay(a);
	_aligned_free(a);
}

static void xsdelay(SDELAY a)
{
	for (uint32_t i = 0; i < a->bsize; i++)
	{
		a->buff[2 * a->inpos + 0] = a->in[2 * i + 0];
		a->buff[2 * a->inpos + 1] = a->in[2 * i + 1];
		a->out[2 * i + 0] = a->buff[2 * a->outpos + 0];
		a->out[2 * i + 1] = a->buff[2 * a->outpos + 1];
		if (++(a->inpos) >= a->size) a->inpos = 0;
		if (++(a->outpos) >= a->size) a->outpos = 0;
	}
}

static void flush_sdelay(SDELAY a)
{
	memset(a->buff, 0, a->size * sizeof(complex));
	a->outpos = 0;
	a->inpos = a->outpos + a->samp_delay;
}


/********************************************************************************************************
*																										*
*                                     STRUCTURE & BASIC DEMODULATION									*
*																										*
********************************************************************************************************/

// [STEREO PERFORMANCE, measured 2026-11 on this code.  Test signal:  a standard MPX with L only
//  (R = 0), 10% pilot, 75 kHz peak deviation, no pre-emphasis (de-emphasis OFF), generated at 1536K
//  and decimated to the DSP rate by the HBResampler, as RXA does.  Tones from 100 Hz to 15 kHz.
//  Separation = L / R at the tone; THD+N = everything in L from 20 Hz to 15 kHz except the tone.
//
//      DSP rate    input bandwidth                       modulation    separation      THD+N
//      192K        180 kHz (HBResampler, +/-90)             100%         36 dB          -39 to  -45 dB
//                                                            50%       57 to  72 dB     -59 to  -79 dB
//      384K        256 kHz (input filter, +/-128)           100%       55 to  65 dB     -61 to  -67 dB
//                                                            50%       87 to  99 dB     -92 to -100 dB
//
//  A stereo signal's Carson's-rule bandwidth is 2 * (75 + 53) = 256 kHz.  At 192K the input band is
//  narrower than that, and the missing sidebands set both the separation and the distortion.  At
//  384K the input filter (see calc_wbfm()) sets the band to 256 kHz to keep out the first adjacent
//  channel.  Without it (+/-180 kHz), separation was 99 to 128 dB and THD+N -109 to -131 dB at
//  100% modulation; there the decoder itself sets the limits, and the largest remaining term is
//  the L+R / L-R droop mismatch described at the 23-53 kHz filter (about 96 dB at 15 kHz).
//  Before the 2026-11 changes:  about 26 dB separation at 192K (16 dB at 50 Hz) with THD+N of
//  -25 to -28 dB (from the squelch-gain modulation), and no stereo at 384K, where the squelch also
//  stayed closed.]

static void discriminator (WBFM a)
{
	double Inew = 0.0;
	double Qnew = 0.0;
	double Iold = 0.0;
	double Qold = 0.0;
	double phase_diff = 0.0;
	double out_val = 0.0;
	double* in = a->run_filin ? a->filin_out : a->in;
	for (int i = 0; i < a->size; i++)
	{
		Inew = in[2 * i + 0];
		Qnew = in[2 * i + 1];
		Iold = a->discSave_I;
		Qold = a->discSave_Q;
		phase_diff = atan2(Iold * Qnew - Inew * Qold, Inew * Iold + Qnew * Qold);
		out_val = a->disc_gain_comp * phase_diff;
		a->discSave_I = Inew;
		a->discSave_Q = Qnew;
		a->disc_out[2 * i + 0] = 2.0 * out_val;
		a->disc_out[2 * i + 1] = 0.0;
	}
}

// [DC block, moved 2026-11 from the discriminator output to the L and R outputs of the matrix.
//  At the discriminator output, the DC block's high-pass response applied to the L+R audio,
//  but NOT to the L-R audio, which is carried at 38 kHz where the DC block is flat.  At low
//  audio frequencies the high-pass phase shift (atan(fc/f)) then mismatched L+R and L-R; at
//  192K (fc = 15.3 Hz) separation was limited to 16 dB at 50 Hz, 22 dB at 100 Hz, 31 dB at
//  300 Hz, and 42 dB at 1 kHz; at 384K, where the fixed R = 0.9995 doubled fc to 30.6 Hz, to
//  11, 16, 26, and 36 dB.  Applying identical DC blocks to L and R leaves L+R and L-R matched.
//  Nothing ahead of the matrix needs the DC removed:  the pilot, L-R, and squelch noise filters
//  are all bandpass.  'R' is now set from a corner frequency in Hz, 'dcb_fc', so that the
//  audio response is the same at any rate.]
static void dc_block(int size, double R, double* buff, double* xsave, double* ysave)
{
	double x   = 0.0;
	double xm1 = 0.0;
	double y   = 0.0;
	double ym1 = 0.0;
	for (int i = 0; i < size; i++)
	{
		x = buff[i];
		xm1 = *xsave;
		ym1 = *ysave;
		y = x - xm1 + R * ym1;
		*xsave = x;
		*ysave = y;
		buff[i] = y;
	}
}

static void dsb_demod (int size, double* inPilot, double* inDSB, double* outLmR)
{
	double square[2] = { 0.0 };
	double temp = 0;
	for (int i = 0; i < 2 * size; i += 2)
	{
		square[0] = inPilot[i + 0] * inPilot[i + 0] - inPilot[i + 1] * inPilot[i + 1];
		square[1] = 2.0 * inPilot[i + 0] * inPilot[i + 1];
		temp = square[0];
		square[0] = +square[1];
		square[1] = +temp;
		outLmR[i + 0] = square[0] * inDSB[i + 0] - square[1] * inDSB[i + 1];
		outLmR[i + 1] = square[0] * inDSB[i + 1] + square[1] * inDSB[i + 0];
	}
}

static void mx(int size, int stereo, double* LpR_in, double* LmR_in, double* L, double* R, double sqgain)
{
	if(stereo)
		for (int i = 0; i < size; i++)
		{
			L[i] = sqgain * (LpR_in[2 * i + 0] + LmR_in[2 * i + 0]);
			R[i] = sqgain * (LpR_in[2 * i + 0] - LmR_in[2 * i + 0]);
		}
	else
		for (int i = 0; i < size; i++)
		{
			L[i] = sqgain * LpR_in[2 * i + 0];
			R[i] = sqgain * LpR_in[2 * i + 0];
		}
}

static void combine (int size, double* L, double* R, double* out)
{
	for (int i = 0; i < size; i++)
	{
		out[2 * i + 0] = L[i];
		out[2 * i + 1] = R[i];
	}
}

static int calc_nc (int nc192, double rate, int size)
{
	// [Added 2026-11.  The filter lengths were chosen at a 192K rate.  With a FIXED number of
	//  taps, a higher rate halves the time span of each filter and doubles its transition widths
	//  in Hz.  At 384K, the 50 Hz wide pilot filter then lost so much gain at 19 kHz that
	//  'mag19' fell from 0.098 to 0.055, below the 0.06 thresholds:  the stereo indicator never
	//  turned on, and the squelch stayed closed ('pilot present but not good').  Scaling the
	//  length with the rate keeps each filter's response in Hz, and therefore 'mag19', the same
	//  at any rate.  The length is rounded up to a power of two and is at least 'size', as
	//  fircore requires.  At 192K the lengths are unchanged (4096, 8192, 4096); at 384K they
	//  are 8192, 16384, 8192.]
	int nc = 1;
	double target = (double)nc192 * (rate / 192000.0);
	while (nc < target) nc <<= 1;
	return (nc > size) ? nc : size;
}

static void calc_wbfm (WBFM a)
{
	double* impulse = NULL;

	// input filter:  +/- 128 kHz, ON at 384K and higher
	// [Added 2026-11.  At 192K the HBResampler limits the input to +/- 90 kHz, which keeps out
	//  a station on the first adjacent channel (200 kHz away).  At 384K it passes +/- 180 kHz,
	//  and that station gets in:  with it 10 dB weaker than the wanted signal, THD+N of the
	//  wanted audio was -37 dB, against -67 dB at 192K.  This filter limits the input to
	//  256 kHz, the Carson's-rule bandwidth of a stereo signal, 2 * (75 + 53) kHz, and gives
	//  -72 dB in the same test.  It is here, rather than using RXA's nbp0, because nbp0's
	//  passband, notches, and phase are console settings.  In particular, the console's
	//  minimum-phase ('low latency') option would put a minimum-phase filter ahead of the
	//  discriminator; at 256 kHz and 100% modulation, that reduced separation from 55-65 dB
	//  to 22-28 dB, and THD+N from about -65 dB to -30 to -37 dB.  This filter is always linear
	//  phase, has no notches, and follows the rate.  It is OFF below 384K, where +/- 128 kHz
	//  would exceed the Nyquist frequency (at 192K, fir_bandpass() would fold it to +/- 64 kHz).
	//  Note:  RXA's S-meter reads ahead of this filter, so at 384K it includes everything
	//  within +/- 180 kHz.]
	a->run_filin = (a->rate >= 384000.0);
	a->flow_filin = -128000.0;
	a->fhigh_filin = +128000.0;
	a->nc_filin = calc_nc(1024, a->rate, a->size);						// 2048 at 384K
	a->filin_out = (double*)malloc0(2 * a->size * sizeof(complex));
	a->pfilin = NULL;
	if (a->run_filin)
	{
		impulse = fir_bandpass(a->nc_filin, a->flow_filin, a->fhigh_filin, a->rate, 0, 1, 1.0 / (2.0 * a->size));
		a->pfilin = create_fircore(a->size, a->in, a->filin_out, a->nc_filin, 0, 8, impulse);
		_aligned_free(impulse);
	}

	// internal buffers
	a->disc_out  = (double*)malloc0(    a->size * sizeof(complex));
	a->fil19_out = (double*)malloc0(2 * a->size * sizeof(complex));
	a->fil53_out = (double*)malloc0(2 * a->size * sizeof(complex));
	a->LmR       = (double*)malloc0(    a->size * sizeof(complex));
	a->LpR       = (double*)malloc0(2 * a->size * sizeof(complex));
	a->L         = (double*)malloc0(    a->size * sizeof(double));
	a->R         = (double*)malloc0(    a->size * sizeof(double));

	// discriminator & dcb
	a->discSave_I = 0.0;
	a->discSave_Q = 0.0;
	a->dcb_fc = 15.0;													// close to the former R = 0.9995 at 192K (15.3 Hz)
	a->dcb_R  = exp(-TWOPI * a->dcb_fc / a->rate);
	a->dcbSave_x[0] = a->dcbSave_x[1] = 0.0;
	a->dcbSave_y[0] = a->dcbSave_y[1] = 0.0;
	a->disc_gain_comp = a->rate / (TWOPI * 75000.0);

	// filter:  0-15 kHz
	a->flow_fil0_15 = 0.0;
	a->fhigh_fil0_15 = 15200.0;
	a->nc_fil0_15 = calc_nc(4096, a->rate, a->size);
	impulse = fir_bandpass(a->nc_fil0_15, a->flow_fil0_15, a->fhigh_fil0_15, a->rate, 0, 1, 1.0 / (2.0 * a->size));
	a->pfil0_15 = create_fircore(a->size, a->disc_out, a->LpR, a->nc_fil0_15, 0, 8, impulse);
	_aligned_free(impulse);

	// filter:  19 kHz
	a->flow_fil19 = 18975.0;
	a->fhigh_fil19 = 19025.0;
	a->nc_fil19 = calc_nc(8192, a->rate, a->size);
	a->magComp_fil19 = 1.495;
	impulse = fir_bandpass(a->nc_fil19, a->flow_fil19, a->fhigh_fil19, a->rate, 0, 1, a->magComp_fil19 / (2.0 * a->size));
	a->pfil19 = create_fircore(a->size, a->disc_out, a->fil19_out, a->nc_fil19, 0, 8, impulse);
	_aligned_free(impulse);

	// filter:  23-53 kHz
	a->flow_fil23_53 = 22800.0;
	a->fhigh_fil23_53 = 53200.0;
	a->nc_fil23_53 = calc_nc(4096, a->rate, a->size);
	// [L-R droop compensation, added 2026-11.  The discriminator measures the phase change
	//  across ONE sample, which is the average of the instantaneous frequency over that sample.
	//  That one-sample average has the response sinc(f/rate) = sin(PI*f/rate) / (PI*f/rate),
	//  which droops toward high frequencies.  The L+R audio loses little, 0.09 dB at 15 kHz
	//  at a 192K rate.  However, the L-R subcarrier at 38 kHz loses 0.57 dB at 192K and
	//  0.14 dB at 384K.  Since L = (L+R) + (L-R) and R = (L+R) - (L-R), that gain mismatch
	//  alone limits stereo separation to about 30 dB at 192K and 42 dB at 384K.  Scaling
	//  this filter by 1/sinc(38000/rate) restores the match.  The two sidebands at 38 +/- f kHz
	//  average to very nearly sinc(38/rate) * sinc(f/rate), so this single constant also follows
	//  the L+R droop across the audio band; the mismatch that remains limits separation to
	//  no less than 71 dB at 192K and 95 dB at 384K (at 15 kHz; far better at lower audio
	//  frequencies).  The pilot needs no compensation because the pilot AGC sets its level.]
	{
		double x = PI * 38000.0 / a->rate;
		a->magComp_fil23_53 = x / sin(x);
	}
	impulse = fir_bandpass(a->nc_fil23_53, a->flow_fil23_53, a->fhigh_fil23_53, a->rate, 0, 1, a->magComp_fil23_53 / (2.0 * a->size));
	a->pfil23_53 = create_fircore(a->size, a->disc_out, a->fil53_out, a->nc_fil23_53, 0, 8, impulse);
	_aligned_free(impulse);

	// de-emphasis filters
	a->dmphL = create_dmph(a->dmph, a->rate, a->dmph_type, a->size, a->L, a->L);
	a->dmphR = create_dmph(a->dmph, a->rate, a->dmph_type, a->size, a->R, a->R);

	// Pilot AGC
	a->tau_attack = 0.001;
	a->n_tau = 4.0;
	a->targ_mult = 0.9999;
	a->out_targ = 1.0 / (a->targ_mult * (1.0 - exp(-a->n_tau)));
	a->agc_delay = (int)ceil(a->rate * a->n_tau * a->tau_attack);
	a->pAGC_Pilot = create_wcpagc(
		1,											// run - always ON
		5,											// mode
		1,											// 0 for max(I,Q), 1 for envelope
		a->fil19_out,								// input buff pointer
		a->fil19_out,								// output buff pointer
		a->size,									// io_buffsize
		(int)a->rate,								// sample rate
		a->tau_attack,								// tau_attack
		2.000,										// tau_decay
		(int)a->n_tau,								// n_tau
		1000.0,										// max_gain (sets threshold, initial value)
		1.0,										// var_gain / slope
		1.0,										// fixed_gain
		1.0,										// max_input
		a->out_targ,								// out_targ
		0.250,										// tau_fast_backaverage
		0.004,										// tau_fast_decay
		4.0,										// pop_ratio
		0,											// hang_enable
		0.500,										// tau_hang_backmult
		0.500,										// hangtime
		2.000,										// hang_thresh
		0.100);										// tau_hang_decay

	// Indicator
	a->pIndy = create_indy(a->size, a->rate, a->fil19_out);

	// Squelch
	a->psql = create_wbsquelch(a->disc_out, a->rate, a->size);

	// Matching Delays
	int delsamps15 = a->agc_delay + (a->nc_fil19 - a->nc_fil0_15) / 2;
	int delsize15 = delsamps15 + a->size;
	int delsamps53 = a->agc_delay + (a->nc_fil19 - a->nc_fil23_53) / 2;
	int delsize53 = delsamps53 + a->size;
	a->del15 = create_sdelay(delsize15, delsamps15, a->size, a->LpR, a->LpR);
	a->del53 = create_sdelay(delsize53, delsamps53, a->size, a->fil53_out, a->fil53_out);
}

static void decalc_wbfm (WBFM a)
{
	destroy_sdelay(a->del53);
	destroy_sdelay(a->del15);
	destroy_wbsquelch(a->psql);
	destroy_indy(a->pIndy);
	destroy_wcpagc(a->pAGC_Pilot);
	destroy_dmph(a->dmphL);
	destroy_dmph(a->dmphR);
	destroy_fircore(a->pfil23_53);
	destroy_fircore(a->pfil19);
	destroy_fircore(a->pfil0_15);
	_aligned_free(a->R);
	_aligned_free(a->L);
	_aligned_free(a->LpR);
	_aligned_free(a->LmR);
	_aligned_free(a->fil53_out);
	_aligned_free(a->fil19_out);
	_aligned_free(a->disc_out);
	if (a->pfilin) destroy_fircore(a->pfilin);
	_aligned_free(a->filin_out);
}

WBFM create_wbfm (int run, int size, double* in, double* out, double rate)
{
	WBFM a = (WBFM)malloc0(sizeof(wbfm));
	// the basics
	a->run = run;
	a->size = size;
	a->in = in;
	a->out = out;
	a->rate = rate;
	a->stereo = 0;
	a->sqgain = 0.0;
	a->dmph = 1;
	a->dmph_type = 0;
	calc_wbfm(a);
	return a;
}

void destroy_wbfm(WBFM a)
{
	decalc_wbfm(a);
	_aligned_free(a);
}

void flush_wbfm(WBFM a)
{
	if (a->pfilin) flush_fircore(a->pfilin);
	memset(a->filin_out, 0, 2 * a->size * sizeof(complex));
	memset(a->disc_out,  0,     a->size * sizeof(complex));
	memset(a->fil19_out, 0, 2 * a->size * sizeof(complex));
	memset(a->fil53_out, 0, 2 * a->size * sizeof(complex));
	memset(a->LmR,       0,     a->size * sizeof(complex));
	memset(a->LpR,       0, 2 * a->size * sizeof(complex));
	memset(a->L,         0,     a->size * sizeof(double));
	memset(a->R,         0,     a->size * sizeof(double));
	flush_dmph(a->dmphL);
	flush_dmph(a->dmphR);
	flush_sdelay(a->del15);
	flush_sdelay(a->del53);
	a->discSave_I = 0.0;
	a->discSave_Q = 0.0;
	a->dcbSave_x[0] = a->dcbSave_x[1] = 0.0;
	a->dcbSave_y[0] = a->dcbSave_y[1] = 0.0;
	flush_fircore(a->pfil0_15);
	flush_fircore(a->pfil19);
	flush_fircore(a->pfil23_53);
	flush_wcpagc(a->pAGC_Pilot);
	flush_indy(a->pIndy);
}

void xwbfm(WBFM a)
{
	if (a->run)
	{
		if (a->run_filin) xfircore(a->pfilin);
		discriminator(a);
		xfircore(a->pfil0_15);
		xfircore(a->pfil19);
		xfircore(a->pfil23_53);
		xindy(a->pIndy, &a->stereo, &a->mag19);					// EXECUTE BEFORE AGC ADJUSTS AMPLITUDE!
		xwbsquelch(a->psql, a->mag19, &a->sqgain);
		xwcpagc(a->pAGC_Pilot);
		xsdelay(a->del15);
		xsdelay(a->del53);
		dsb_demod(a->size, a->fil19_out, a->fil53_out, a->LmR);
		mx(a->size, a->stereo, a->LpR, a->LmR, a->L, a->R, a->sqgain);
		dc_block(a->size, a->dcb_R, a->L, &a->dcbSave_x[0], &a->dcbSave_y[0]);
		dc_block(a->size, a->dcb_R, a->R, &a->dcbSave_x[1], &a->dcbSave_y[1]);
		xdmph(a->dmphL);
		xdmph(a->dmphR);
		combine(a->size, a->L, a->R, a->out);
	}
	else if (a->in != a->out)
		memcpy(a->out, a->in, a->size * sizeof(complex));
}

void setBuffers_wbfm(WBFM a, double* in, double* out)
{
	a->in = in;
	a->out = out;
	if (a->pfilin) setBuffers_fircore(a->pfilin, a->in, a->filin_out);
}

void setSamplerate_wbfm(WBFM a, int rate)
{
	decalc_wbfm(a);
	a->rate = rate;
	calc_wbfm(a);
}

void setSize_wbfm(WBFM a, int size)
{
	decalc_wbfm(a);
	a->size = size;
	calc_wbfm(a);
}

void setRun_wbfm (WBFM a, int run)
{
	a->run = run;
}


/********************************************************************************************************
*																										*
*											RXA Properties												*
*																										*
********************************************************************************************************/


PORT
void SetRXAWBFMdmph(int channel, int dmph_run, int dmph_continent)
{
	WBFM a = rxa[channel].wbfm.p;
	if ((a->dmph != dmph_run) || (a->dmph_type != dmph_continent))
	{
		EnterCriticalSection(&ch[channel].csDSP);
		a->dmph = dmph_run;
		a->dmph_type = dmph_continent;
		setValues_dmph(a->dmphL, a->dmph, a->dmph_type);		// [added 2026-11; see setValues_dmph()]
		setValues_dmph(a->dmphR, a->dmph, a->dmph_type);
		LeaveCriticalSection(&ch[channel].csDSP);
	}
}

PORT
int GetRXAWBFMStereoIndicator(int channel)
{
	WBFM a = rxa[channel].wbfm.p;
	int stereo = 0;
	EnterCriticalSection(&ch[channel].csDSP);
	stereo = a->stereo;
	LeaveCriticalSection(&ch[channel].csDSP);
	return stereo;
}