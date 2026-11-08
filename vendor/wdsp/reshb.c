/*  reshb.c

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
#include "comm.h"

typedef struct _hbres
{
    int N;
    double* h;
    complex_t* ring;
    int ring_ptr;
    complex_t* in;
    complex_t* out;
    int size;
    int L;              // [2026-11:  delay-line length, in input samples;  'ring' holds 2 * L]
    int interp;         // [2026-11:  0 => decimate by 2;  1 => interpolate by 2]
} hbres, * HBRES;

struct HBRESdata 
{
    uint32_t run;
    complex_t* in;
    complex_t* out;
    uint32_t insize;
    uint32_t inrate;
    uint32_t outrate;
    uint32_t nStages;
    uint32_t taps[10];
    uint32_t nBuffs;
    uint32_t bsize[10];
    complex_t* buff[10];
    hbres rsmps[10];
};

static double* dcheb_compute(int N, double atten_db)
{
    double* s_dcheb_buf = (double*)malloc0(N * sizeof(double));
    int    order = N - 1;
    double beta = pow(10.0, atten_db / 20.0);
    double x0 = cosh(acosh(beta) / (double)order);
    double* W = (double*)malloc0(N * sizeof(double));
    for (int k = 0; k < N; k++)
    {
        double x = x0 * cos((PI * k) / (double)N);
        if (x > 1.0)
            W[k] = cosh((double)order * acosh(x));
        else if (x < -1.0)
            W[k] = (order % 2 == 0 ? 1.0 : -1.0)
            * cosh((double)order * acosh(-x));
        else
            W[k] = cos((double)order * acos(x));
    }
    int shift = (N + 1) / 2;
    for (int n = 0; n < N; n++)
    {
        double sum = 0.0;
        for (int k = 0; k < N; k++)
            sum += W[k] * cos((2.0 * PI * k * (n + shift)) / (double)N);
        s_dcheb_buf[n] = sum / (double)N;
    }
    int    M = (N - 1) / 2;
    double peak = s_dcheb_buf[M];
    for (int n = 0; n < N; n++)
        s_dcheb_buf[n] /= peak;
    _aligned_free(W);
    return s_dcheb_buf;
}

static void impulse_halfband_dcheb(double* h, int N, double atten_db)
{
    assert(N % 2 == 1);

    double* window = NULL;
    window = dcheb_compute(N, atten_db);

    int    M = (N - 1) / 2;
    double omega_c = PI / 2.0;

    for (int i = 0; i < N; i++)
    {
        int k = i - M;
        if (k == 0)
            h[i] = 0.5;
        else if (abs(k) % 2 == 0)
            h[i] = 0.0;
        else
        {
            double sinc = sin(omega_c * (double)k) / (PI * (double)k);
            h[i] = sinc * window[i];
        }
    }
    _aligned_free(window);
}



static double bh7 (int n, int N) 
{
    // For ODD 'N'
    const double a0 = 0.27105140069342;
    const double a1 = 0.43329793923448;
    const double a2 = 0.21812299954311;
    const double a3 = 0.06592544638803;
    const double a4 = 0.01081174209837;
    const double a5 = 0.00077658482522;
    const double a6 = 0.00001388721735;
    double arg = (TWOPI * (double)n) / (double)(N - 1);
    double w = a0
        - a1 * cos(arg)
        + a2 * cos(2.0 * arg)
        - a3 * cos(3.0 * arg)
        + a4 * cos(4.0 * arg)
        - a5 * cos(5.0 * arg)
        + a6 * cos(6.0 * arg);
    return w;
}

static void impulse_halfband (double* h, int N)
{
    assert (N % 2 == 1);
    int M = (N - 1) / 2;
    int i = 0, k = 0;
    double sinc = 0.0;
    double omega_c = PI / 2.0;
    for (i = 0; i < N; i++)
    {
        k = i - M;
        if (k == 0)           h[i] = 0.5;
        else if (abs(k) % 2 == 0)  h[i] = 0.0;
        else
        {
            sinc = sin (omega_c * (double)k) / (PI * (double)k);
            h[i] = sinc * bh7 (i, N);
        }
    }
}

// ------------------------------------------------------------------------------
// [2026-11]  THE STAGES:  DECIMATE BY 2, OR INTERPOLATE BY 2.
//
// DOUBLED DELAY LINE.  Each input sample is written twice, at ring[p] and at
// ring[p + L], where L is the delay-line length.  The L most recent samples are
// then always contiguous, ending at w = ring + p + L:  w[0] is the newest sample
// and w[-k] the one k samples older, for k = 0 ... L - 1, with no modulo
// arithmetic on any index.  Through 2.11 each tap computed two ring indices with
// two '%' operations each, and that cost far more than the multiplies:  the
// doubled line makes a stage 6 (19 taps) to 10 (319 taps) times faster.  The
// sums are formed in exactly the same order as before, so the output of every
// decimating stage is bit-for-bit the same.
//
// THE FOLDED SUM.  The halfband taps are symmetric about the centre tap M, and
// apart from the centre tap (0.5) only the taps an odd distance j from it are
// non-zero.  Both kinds of stage therefore compute the same sum,
//     sum over odd j of  h[M - j] * (older sample + newer sample),
// and hb_fold () does that for both.  The pairs of samples are 2 apart in the
// full-rate delay line of a decimating stage ('step' = 2), and 1 apart in the
// input-rate delay line of an interpolating stage ('step' = 1).
// ------------------------------------------------------------------------------

static __inline void hb_fold (complex_t* acc, const complex_t* pl, const complex_t* pr,
    const double* ph, int nt, int step)
{   // pl:  first 'newer' sample (moves toward newer);  pr:  first 'older' sample (moves
    // toward older);  ph:  the tap for that pair (moves 2 taps toward the end of h)
    for (int t = 0; t < nt; t++)
    {
        double hval = ph[-2 * t];
        acc->i += (pl[step * t].i + pr[-step * t].i) * hval;
        acc->q += (pl[step * t].q + pr[-step * t].q) * hval;
    }
}

static void xhbres_dec (HBRES r)
{   // decimate by 2:  'size' input samples in, size / 2 out.  L = N.
    int n_out = 0;
    int L = r->L;
    int center = (r->N - 1) / 2;
    int nt = (center + 1) / 2;
    for (int i = 0; i < r->size; i++)
    {
        r->ring[r->ring_ptr] = r->ring[r->ring_ptr + L] = r->in[i];
        if (i % 2 == 1)
        {
            const complex_t* w = r->ring + r->ring_ptr + L;
            complex_t accum;
            accum.i = w[-center].i * r->h[center];
            accum.q = w[-center].q * r->h[center];
            hb_fold (&accum, w - (center - 1), w - (center + 1), r->h + center - 1, nt, 2);
            r->out[n_out++] = accum;
        }
        if (++r->ring_ptr >= L) r->ring_ptr = 0;
    }
}

// ------------------------------------------------------------------------------
// [2026-11]  INTERPOLATE BY 2.
//
// Zero-stuffing doubles the rate:  u[2n] = x[n], u[2n + 1] = 0.  Filtering u with
// g = 2h (the 2 restores the gain that zero-stuffing halves) gives two outputs per
// input sample.  The centre index M = (N - 1) / 2 is odd for the lengths used here
// (N = 4k + 3), so:
//     y[2n]     = sum over odd j of  g[M - j] * (x[n - (M - j) / 2] + x[n - (M + j) / 2])
//     y[2n + 1] = g[M] * x[n - (M - 1) / 2]  =  x[n - (M - 1) / 2]
// The second output is a copy of a delayed input sample (g[M] = 1.0 exactly), so an
// interpolating stage costs the same multiplies per input sample as a decimating
// stage per output sample.  The taps are stored as g = 2h (exact in floating point),
// so the sum needs no extra multiply.  The delay line holds input samples, and
// L = M + 1 = (N + 1) / 2 of them reach back to x[n - M].
// ------------------------------------------------------------------------------

static void xhbres_int (HBRES r)
{   // interpolate by 2:  'size' input samples in, 2 * size out.  L = (N + 1) / 2.
    int n_out = 0;
    int L = r->L;
    int M = (r->N - 1) / 2;
    int nt = (M + 1) / 2;
    for (int i = 0; i < r->size; i++)
    {
        r->ring[r->ring_ptr] = r->ring[r->ring_ptr + L] = r->in[i];
        const complex_t* w = r->ring + r->ring_ptr + L;
        complex_t accum = { 0.0, 0.0 };
        hb_fold (&accum, w - (M - 1) / 2, w - (M + 1) / 2, r->h + M - 1, nt, 1);
        r->out[n_out++] = accum;
        r->out[n_out++] = w[-(M - 1) / 2];
        if (++r->ring_ptr >= L) r->ring_ptr = 0;
    }
}

void xHBResampler(HBResampler tData)
{
    if (tData->run)
    {
        for (uint32_t i = 0; i < tData->nStages; i++)
        {
            if (tData->rsmps[i].interp)
                xhbres_int(&(tData->rsmps[i]));
            else
                xhbres_dec(&(tData->rsmps[i]));
        }
    }
    else
        memcpy(tData->out, tData->in, tData->insize * sizeof(complex_t));
}

// ------------------------------------------------------------------------------
// [2026-11]  THE RATE PAIRS.
//
// Taps for each stage, first stage first.  All the designs use the Dolph-
// Chebyshev window at 140 dB;  the usable band is +/- 0.46875 * (the lower rate),
// e.g., 180 kHz at 192 kHz, where the 319-tap stage is down 0.001 dB at the edge.
//
// DECIMATION, to 192k, 96k and 48k:  the designs from 2.11, unchanged.  Modeled
// over the usable band (worst alias anywhere in it, from the exact response of each
// stage), they give 100 - 104 dB of alias rejection for every multi-stage pair and
// 122 dB for the single-stage pairs, with less than 0.0003 dB of passband droop;
// tones through this code agree with the model to within 0.14 dB at -170 dB.
// The 192k pairs use the fewest taps that hold 100 dB:  each stage 4 taps shorter
// falls below it, which is why 1536k -> 192k needs 27 taps in its first stage
// where 3072k -> 192k manages with 23 at 1536k (its first stage helps).
//
// DECIMATION, to 384k:  the 192k designs one octave up.  Every frequency scales
// with the rates, so each new pair is exactly as good as its 192k counterpart.
//
// INTERPOLATION:  each decimation design in reverse order (e.g., 48k -> 768k is
// 768k -> 48k backwards:  the long 319-tap stage first, at the lowest rate, where
// it sets the band edge).  An interpolator is the transpose of the decimator, so its
// image rejection is the same as the decimator's alias rejection.
//
// PERFORMANCE OF EACH PAIR.  'band' is the usable band, +/- 0.46875 * the lower rate.
// The rejection is the worst alias (decimation) or image (interpolation) anywhere in
// that band, re the wanted signal;  'ripple' is the peak-to-peak variation of the gain
// across it (mostly the droop toward the edges).  [From the exact response of each
// stage, at 20001 frequencies across the band.  Away from the band edges the rejection
// is higher:  e.g., 768k -> 192k holds 140 dB to +/- 80 kHz, and 120 dB to +/- 84.5 kHz.]
//
//     decimation       band (kHz)   alias rej (dB)   ripple (dB)
//     6144 ->  384 kHz         360            100.3       0.00019
//     3072 ->  384 kHz         360            103.6       0.00007
//     1536 ->  384 kHz         360            102.7       0.00007
//      768 ->  384 kHz         360            121.7       0.00001
//     6144 ->  192 kHz         180            100.4       0.00021
//     3072 ->  192 kHz         180            100.3       0.00019
//     1536 ->  192 kHz         180            103.6       0.00007
//      768 ->  192 kHz         180            102.7       0.00007
//      384 ->  192 kHz         180            121.7       0.00001
//     6144 ->   96 kHz          90            103.1       0.00008
//     3072 ->   96 kHz          90            103.1       0.00007
//     1536 ->   96 kHz          90            103.1       0.00007
//      768 ->   96 kHz          90            103.1       0.00007
//      384 ->   96 kHz          90            102.7       0.00007
//      192 ->   96 kHz          90            121.7       0.00001
//     6144 ->   48 kHz          45            103.1       0.00007
//     3072 ->   48 kHz          45            103.1       0.00007
//     1536 ->   48 kHz          45            103.1       0.00007
//      768 ->   48 kHz          45            103.1       0.00007
//      384 ->   48 kHz          45            103.1       0.00007
//      192 ->   48 kHz          45            102.7       0.00007
//       96 ->   48 kHz          45            121.7       0.00001
//
//     interpolation    band (kHz)   image rej (dB)   ripple (dB)
//       48 ->   96 kHz          45            121.7       0.00001
//       48 ->  192 kHz          45            102.7       0.00007
//       48 ->  384 kHz          45            103.1       0.00007
//       48 ->  768 kHz          45            103.1       0.00007
//       48 -> 1536 kHz          45            103.1       0.00007
//       48 -> 3072 kHz          45            103.1       0.00007
//       48 -> 6144 kHz          45            103.1       0.00007
//       96 ->  192 kHz          90            121.7       0.00001
//       96 ->  384 kHz          90            102.7       0.00007
//       96 ->  768 kHz          90            103.1       0.00007
//       96 -> 1536 kHz          90            103.1       0.00007
//       96 -> 3072 kHz          90            103.1       0.00007
//       96 -> 6144 kHz          90            103.1       0.00008
//      192 ->  384 kHz         180            121.7       0.00001
//      192 ->  768 kHz         180            102.7       0.00007
//      192 -> 1536 kHz         180            103.6       0.00007
//      192 -> 3072 kHz         180            100.3       0.00019
//      192 -> 6144 kHz         180            100.4       0.00021
//      384 ->  768 kHz         360            121.7       0.00001
//      384 -> 1536 kHz         360            102.7       0.00007
//      384 -> 3072 kHz         360            103.6       0.00007
//      384 -> 6144 kHz         360            100.3       0.00019
// ------------------------------------------------------------------------------

typedef struct _hbpair
{
    uint32_t inrate;
    uint32_t outrate;
    uint32_t nStages;
    uint32_t taps[7];
} hbpair;

static const hbpair hbpairs[] =
{
    // decimation, from 2.11
    { 6144000,  192000, 5, {  19,  23,  23,  35, 319 } },
    { 6144000,   96000, 6, {  19,  23,  23,  35,  35, 319 } },
    { 6144000,   48000, 7, {  19,  23,  23,  35,  35,  35, 319 } },
    { 3072000,  192000, 4, {  23,  23,  35, 319 } },
    { 3072000,   96000, 5, {  23,  23,  35,  35, 319 } },
    { 3072000,   48000, 6, {  23,  23,  35,  35,  35, 319 } },
    { 1536000,  192000, 3, {  27,  35, 319 } },
    { 1536000,   96000, 4, {  23,  35,  35, 319 } },
    { 1536000,   48000, 5, {  23,  35,  35,  35, 319 } },
    {  768000,  192000, 2, {  35, 319 } },
    {  768000,   96000, 3, {  35,  35, 319 } },
    {  768000,   48000, 4, {  35,  35,  35, 319 } },
    {  384000,  192000, 1, { 319 } },
    {  384000,   96000, 2, {  35, 319 } },
    {  384000,   48000, 3, {  35,  35, 319 } },
    {  192000,   96000, 1, { 319 } },
    {  192000,   48000, 2, {  35, 319 } },
    {   96000,   48000, 1, { 319 } },
    // decimation to 384k:  the 192k designs one octave up
    { 6144000,  384000, 4, {  23,  23,  35, 319 } },
    { 3072000,  384000, 3, {  27,  35, 319 } },
    { 1536000,  384000, 2, {  35, 319 } },
    {  768000,  384000, 1, { 319 } },
    // interpolation:  the decimation designs in reverse
    {   48000,   96000, 1, { 319 } },
    {   48000,  192000, 2, { 319,  35 } },
    {   48000,  384000, 3, { 319,  35,  35 } },
    {   48000,  768000, 4, { 319,  35,  35,  35 } },
    {   48000, 1536000, 5, { 319,  35,  35,  35,  23 } },
    {   48000, 3072000, 6, { 319,  35,  35,  35,  23,  23 } },
    {   48000, 6144000, 7, { 319,  35,  35,  35,  23,  23,  19 } },
    {   96000,  192000, 1, { 319 } },
    {   96000,  384000, 2, { 319,  35 } },
    {   96000,  768000, 3, { 319,  35,  35 } },
    {   96000, 1536000, 4, { 319,  35,  35,  23 } },
    {   96000, 3072000, 5, { 319,  35,  35,  23,  23 } },
    {   96000, 6144000, 6, { 319,  35,  35,  23,  23,  19 } },
    {  192000,  384000, 1, { 319 } },
    {  192000,  768000, 2, { 319,  35 } },
    {  192000, 1536000, 3, { 319,  35,  27 } },
    {  192000, 3072000, 4, { 319,  35,  23,  23 } },
    {  192000, 6144000, 5, { 319,  35,  23,  23,  19 } },
    {  384000,  768000, 1, { 319 } },
    {  384000, 1536000, 2, { 319,  35 } },
    {  384000, 3072000, 3, { 319,  35,  27 } },
    {  384000, 6144000, 4, { 319,  35,  23,  23 } },
};

static void calc_HBResampler(HBResampler tData)
{
    const hbpair* pair = NULL;
    for (size_t k = 0; k < sizeof(hbpairs) / sizeof(hbpairs[0]); k++)
        if (hbpairs[k].inrate == tData->inrate && hbpairs[k].outrate == tData->outrate)
        {
            pair = &hbpairs[k];
            break;
        }
    if (pair)
    {
        tData->run = 1;
        tData->nStages = pair->nStages;
        for (uint32_t i = 0; i < pair->nStages; i++)
            tData->taps[i] = pair->taps[i];
        tData->nBuffs = pair->nStages - 1;
    }
    else
    {   // [no pair:  equal rates, or a pair this resampler doesn't do;  the input is copied]
        tData->run = 0;
        tData->nStages = 0;
        tData->taps[0] = 3;
        tData->nBuffs = 0;
    }
    int interp = tData->outrate > tData->inrate;
    const int WINTYPE = 2;
    for (uint32_t i = 0; i < tData->nBuffs; i++)
    {   // [the buffer after stage i:  halving per stage when decimating, doubling when interpolating]
        if (interp)
            tData->bsize[i] = tData->insize << (i + 1);
        else
            tData->bsize[i] = tData->insize >> (i + 1);
    }
    for (uint32_t i = 0; i < tData->nBuffs; i++)
    {
        tData->buff[i] = (complex_t*)malloc0(tData->bsize[i] * sizeof(complex_t));
    }
    for (uint32_t i = 0; i < tData->nStages; i++)
    {
        if (i == 0)
        {
            tData->rsmps[i].in = tData->in;
            tData->rsmps[i].size = tData->insize;
        }
        else
        {
            tData->rsmps[i].in = tData->buff[i - 1];
            tData->rsmps[i].size = tData->bsize[i - 1];
        }
        if (i == tData->nStages - 1)
            tData->rsmps[i].out = tData->out;
        else 
            tData->rsmps[i].out = tData->buff[i];
        tData->rsmps[i].N = tData->taps[i];
        tData->rsmps[i].interp = interp;
        tData->rsmps[i].L = interp ? (tData->rsmps[i].N + 1) / 2 : tData->rsmps[i].N;
        tData->rsmps[i].ring_ptr = 0;
        tData->rsmps[i].ring = (complex_t *)malloc0(2 * tData->rsmps[i].L * sizeof(complex_t));
        tData->rsmps[i].h = (double*)malloc0(tData->rsmps[i].N * sizeof(double));
        if (WINTYPE == 1)
            impulse_halfband(tData->rsmps[i].h, tData->rsmps[i].N);
        else if (WINTYPE == 2)
        {
            double cheb_atten = 140.0;
            impulse_halfband_dcheb(tData->rsmps[i].h, tData->rsmps[i].N, cheb_atten);
        }
        if (interp)
            for (int k = 0; k < tData->rsmps[i].N; k++)
                tData->rsmps[i].h[k] *= 2.0;
    }
}

HBResampler create_HBResampler(uint32_t inrate, uint32_t outrate,
    uint32_t insize, complex_t* in, complex_t* out)
{
    HBResampler tData = (HBResampler)malloc0(sizeof(struct HBRESdata));
    if (!tData) return NULL;
    tData->inrate = inrate;
    tData->outrate = outrate;
    assert(insize % 2 == 0);
    tData->insize = insize;
    tData->in = in;
    tData->out = out;
    calc_HBResampler(tData);
    return tData;
}

static void decalc_HBResampler(HBResampler tData)
{
    for (uint32_t i = 0; i < tData->nStages; i++)
    {
        _aligned_free(tData->rsmps[i].h);
        _aligned_free(tData->rsmps[i].ring);
    }
    for (uint32_t i = 0; i < tData->nBuffs; i++)
    {
        _aligned_free(tData->buff[i]);
    }
}

void destroy_HBResampler(HBResampler tData)
{
    decalc_HBResampler(tData);
    _aligned_free(tData);
}

void flush_HBResampler(HBResampler tData)
{
    for (uint32_t i = 0; i < tData->nStages; i++)
    {
        memset(tData->rsmps[i].ring, 0, 2 * tData->rsmps[i].L * sizeof(complex_t));
        tData->rsmps[i].ring_ptr = 0;
    }
    for (uint32_t i = 0; i < tData->nBuffs; i++)
    {
        memset(tData->buff[i], 0, tData->bsize[i] * sizeof(complex_t));
    }
}

void setBuffers_HBResampler(HBResampler tData, complex_t* in, complex_t* out)
{
    decalc_HBResampler(tData);
    tData->in = in;
    tData->out = out;
    calc_HBResampler(tData);
}

void setSize_HBResampler(HBResampler tData, uint32_t insize)
{
    decalc_HBResampler(tData);
    tData->insize = insize;
    calc_HBResampler(tData);
}

void setInRate_HBResampler(HBResampler tData, int inrate)
{
    decalc_HBResampler(tData);
    tData->inrate = inrate;
    calc_HBResampler(tData);
}

void setOutRate_HBResampler(HBResampler tData, int outrate)
{
    decalc_HBResampler(tData);
    tData->outrate = outrate;
    calc_HBResampler(tData);
}

