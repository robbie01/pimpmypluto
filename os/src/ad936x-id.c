/* ad936x-id -- release the AD936x from reset and read its product ID over
 * spidev.  Smoke test for the userspace radio path (no kernel radio driver). */
#include <fcntl.h>
#include <linux/gpio.h>
#include <linux/spi/spidev.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define GPIO_RESETB 67   /* EMIO 13 */
#define REG_PRODUCT_ID 0x037

static int spi_read(int fd, unsigned reg, uint8_t *val)
{
	uint8_t tx[3] = { (uint8_t)(reg >> 8) & 0x03, (uint8_t)reg, 0 }, rx[3] = { 0 };
	struct spi_ioc_transfer t;
	memset(&t, 0, sizeof t);
	t.tx_buf = (uintptr_t)tx;
	t.rx_buf = (uintptr_t)rx;
	t.len = 3;
	if (ioctl(fd, SPI_IOC_MESSAGE(1), &t) < 0)
		return -1;
	*val = rx[2];
	return 0;
}

int main(void)
{
	struct gpio_v2_line_request req;
	struct gpio_v2_line_values vals = { .bits = 1, .mask = 1 };
	uint8_t id, mode;
	int chip, spi;

	chip = open("/dev/gpiochip0", O_RDONLY);
	memset(&req, 0, sizeof req);
	req.offsets[0] = GPIO_RESETB;
	req.num_lines = 1;
	req.config.flags = GPIO_V2_LINE_FLAG_OUTPUT;
	strcpy(req.consumer, "ad936x-resetb");
	if (chip < 0 || ioctl(chip, GPIO_V2_GET_LINE_IOCTL, &req) < 0) {
		perror("gpio");
		return 1;
	}
	vals.bits = 0; ioctl(req.fd, GPIO_V2_LINE_SET_VALUES_IOCTL, &vals); usleep(10000);
	vals.bits = 1; ioctl(req.fd, GPIO_V2_LINE_SET_VALUES_IOCTL, &vals); usleep(10000);

	spi = open("/dev/spidev0.0", O_RDWR);
	if (spi < 0 || ioctl(spi, SPI_IOC_RD_MODE, &mode) < 0 || spi_read(spi, REG_PRODUCT_ID, &id) < 0) {
		perror("spidev");
		return 1;
	}
	printf("spi mode %u, product id 0x%02x: %s, rev %u\n", mode, id,
	       (id & 0xf8) == 0x08 ? "AD936x" : "unexpected", id & 7);
	return (id & 0xf8) == 0x08 ? 0 : 1;
}
