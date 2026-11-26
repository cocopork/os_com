# rNVPCe create/overwrite paired experiment

This experiment holds the kernel, device, fio configuration, and 2 GiB byte
count constant while changing only whether every file and SSD extent is fully
allocated before the measured 128 KiB sequential write. It runs one and 16
jobs on rNVPCe. The overwrite preparation uses native EXT4 direct I/O, calls
`sync`, unmounts, and then remounts the nojournal volume with rNVPCe enabled.

Run once with the release ISO and once with an otherwise identical ISO built
with `pipeline_profile::ENABLED = true`. Generated logs, CSV files, images,
and plots remain outside Git.

```sh
ART=/home/zn/asterinas-build-env/rnvpce-overwrite-20261030/release \
SOURCE_ART=/home/zn/asterinas-build-env/preliminary-performance-sweep-20261030-current \
./run.sh --authorized-target
```
