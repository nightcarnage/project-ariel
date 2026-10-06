# Build the smiflash kernel module out-of-tree.
#
# On a normal distro with complete kernel headers this Just Works. On a BC-250
# whose headers package ships incomplete (missing autoconf.h) and/or with host
# tools marked x86-64-v4 that the Zen2 CPU can't start, run `./prepare.sh` first
# (the DKMS PRE_BUILD hook does this automatically) to make the tree buildable
# on the board itself — no cross-build host needed.

obj-m := smiflash.o

KDIR ?= /lib/modules/$(shell uname -r)/build

# A clang-built kernel's tree passes clang-only flags (e.g.
# -mstack-alignment=8) that gcc rejects, so detect that via the config marker and
# pass LLVM=1 to use the matching toolchain for both the manual build and the
# DKMS build (which runs this Makefile). The BC-250's shipped kernel is
# gcc-built (CONFIG_CC_IS_GCC=y, CONFIG_LTO_NONE=y on the 7.2.9-1 config), so no
# marker is present and it takes the ordinary path; the detection is kept for
# clang-built kernels.
CLANG_KERNEL := $(wildcard $(KDIR)/include/config/CC_IS_CLANG)
MAKE_EXTRA := $(if $(strip $(CLANG_KERNEL)),LLVM=1,)

all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) $(MAKE_EXTRA) modules

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean 2>/dev/null || rm -f *.o *.ko *.mod* .*.cmd modules.order Module.symvers
