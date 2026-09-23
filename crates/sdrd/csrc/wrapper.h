#include "ad9361_api.h"
#include "ad9361.h"
#include "axi_adc_core.h"
#include "axi_dac_core.h"
#include "no_os_gpio.h"
AD9361_InitParam *pmp_default_init_param(void);
struct axi_dac_init *pmp_tx_dac_init(void);
#include <linux/gpio.h>
