/* The one piece of C that is ours: instantiates ADI's reference
 * AD9361_InitParam (a 270-line positional initializer, kept verbatim in
 * third_party) against our platform, so Rust only overrides what differs. */
#include <stddef.h>
#include "ad9361_api.h"
#include "axi_adc_core.h"
#include "axi_dac_core.h"
#include "linux_spi.h"
#include "no_os_gpio.h"

extern const struct no_os_gpio_platform_ops pmp_gpio_ops;   /* src/platform.rs */

#define GPIO_OPS      &pmp_gpio_ops
#define GPIO_PARAM    NULL
#define SPI_OPS       &linux_spi_ops
#define SPI_PARAM     NULL
#define SPI_DEVICE_ID 0
#define SPI_CS        0

/* Physical addresses; src/platform.rs resolves them to UIO mappings.
 * Two channels (I, Q): the board runs 1R1T, and the channel count is what
 * makes the FPGA's pack/unpack cores lay the DMA stream out as I,Q,I,Q
 * rather than I0,Q0,I1,Q1. */
struct axi_adc_init rx_adc_init = { "cf-ad9361-lpc", 0x79020000, 2, 0, 0 };
/* rate 0: what ad9361_post_setup() itself programs for 1R1T on a CMOS
 * interface (1 for LVDS, 3 for 2R2T).  Anything else makes the DAC core hand
 * the chip samples at the wrong cadence -- a tone comes out at half frequency. */
struct axi_dac_init tx_dac_init = { "cf-ad9361-dds-core-lpc", 0x79024000, 2, NULL, 0 };

#include "ad9361_default_init_param.inc"

AD9361_InitParam *pmp_default_init_param(void) { return &default_init_param; }
struct axi_dac_init *pmp_tx_dac_init(void) { return &tx_dac_init; }
AD9361_RXFIRConfig pmp_rx_fir;   /* zeroed; filled from Rust when a FIR is wanted */
AD9361_TXFIRConfig pmp_tx_fir;
