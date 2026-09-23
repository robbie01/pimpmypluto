// SPDX-License-Identifier: GPL-2.0
/*
 * a9reg -- read, and optionally modify, the Cortex-A9 auxiliary control
 * register and the (undocumented, secure-only) diagnostic control register on
 * every online CPU.  Diagnostic tool for erratum workarounds:
 *
 *   insmod a9reg.ko                          report only
 *   insmod a9reg.ko diag_set=0x400000        set diagnostic bit 22 on all CPUs
 *   insmod a9reg.ko actlr_clr=0x6            clear ACTLR bits 1,2 (prefetch)
 *
 * Loading always "fails" with -EAGAIN after doing its work, so it never stays
 * resident and can simply be insmod'ed again.
 */
#include <linux/module.h>
#include <linux/smp.h>

static unsigned int diag_set, diag_clr, actlr_set, actlr_clr;
module_param(diag_set, uint, 0);
module_param(diag_clr, uint, 0);
module_param(actlr_set, uint, 0);
module_param(actlr_clr, uint, 0);

static void a9reg_one(void *unused)
{
	u32 actlr, diag, actlr2, diag2;

	asm volatile("mrc p15, 0, %0, c1, c0, 1" : "=r"(actlr));
	asm volatile("mrc p15, 0, %0, c15, c0, 1" : "=r"(diag));
	actlr2 = (actlr | actlr_set) & ~actlr_clr;
	diag2 = (diag | diag_set) & ~diag_clr;
	if (actlr2 != actlr)
		asm volatile("mcr p15, 0, %0, c1, c0, 1" : : "r"(actlr2));
	if (diag2 != diag)
		asm volatile("mcr p15, 0, %0, c15, c0, 1" : : "r"(diag2));
	asm volatile("mrc p15, 0, %0, c1, c0, 1" : "=r"(actlr2));
	asm volatile("mrc p15, 0, %0, c15, c0, 1" : "=r"(diag2));
	isb();
	pr_info("a9reg: cpu%d ACTLR %08x -> %08x   DIAG %08x -> %08x\n",
		smp_processor_id(), actlr, actlr2, diag, diag2);
}

static int __init a9reg_init(void)
{
	on_each_cpu(a9reg_one, NULL, 1);
	return -EAGAIN;
}
module_init(a9reg_init);
MODULE_DESCRIPTION("Cortex-A9 ACTLR / diagnostic register peek and poke");
MODULE_LICENSE("GPL");
