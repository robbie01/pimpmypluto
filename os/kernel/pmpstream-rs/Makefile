# Rust core + a C shim for the dmaengine calls that are static inlines (which
# bindgen cannot see) -- there are no dmaengine bindings in the kernel yet.
obj-m := pmpstream.o
pmpstream-y := pmpstream_core.o shim.o
