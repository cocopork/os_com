# Environment

Use host harness entry points on a KVM-capable host with Docker access; the
harness enters its privileged container and launches QEMU with virtio-blk. Do
not invoke guest scripts directly. The shared policy lives in the
[test guide](../../../../test/README.md).

Use one build at a time. Reuse a build only when its stamp proves the kernel and
guest script are unchanged. Give each run exclusive ownership of its QEMU
instance, test device, images, result directory, and container cache. The
harnesses reformat their test filesystem, so destructive confirmation requires
an independently verified device mapping.
