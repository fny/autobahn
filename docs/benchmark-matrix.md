# Benchmark matrix: September–October 2026

This document presents the complete benchmark measurement matrix comparing **Autobahn 1.0.0** against **Mutagen 0.19.0-dev** on Linux. The [summary](./benchmarks.md) explains the headline results. Machine-readable metrics, per-run ranges, exclusions and problem records are in [`benchmarks/2026-10-02.json`](../benchmarks/2026-10-02.json). Raw samples and logs are kept out of the repository.


This is one complete set, refreshed through October 2: every cell, both tools, from identified builds rather than a single run of today's HEAD. The five patch cells were re-run on `0c72865`; every other cell is the base run.

## Provenance & Build Metadata

| Test Cells | Autobahn Build | Run Identifier | Date |
| :--- | :--- | :--- | :---: |
| Base Matrix (all cells except 5 patch runs below) | `e7b3ac0` (1.4.0+e16, musl `dist`, mimalloc) | `bench-1790601586` | 2026-09-28 |
| `chromium-10-fan-patch` | `0c72865` | `bench-1790869509` | 2026-10-01 |
| `chromium-1-patch`, `chromium-10-patch` | `0c72865` | `bench-1790873270` | 2026-10-01 |
| `50k-1-patch`, `50k-10-patch` | `0c72865` | `bench-1790941679` | 2026-10-02 |


**Harness Environment:** Ubuntu 24.04 LTS, 8 cores, 16 GB RAM, 3.5 GHz Intel Xeon 8375C

## Propagation latency

All percentiles are pooled milliseconds across five repeats. `ab` is Autobahn, `mu` is mutagen. Samples and skipped ticks are shown as `ab / mu`. `-bidir` measures each direction separately; `-fan` has ten destinations; `-patch` changes ranges within large files instead of replacing small files. The direction labels retain the corpus identity for two-tree cells.

| Cell | Direction | ab p50 | ab p90 | ab p99 | mu p50 | mu p90 | mu p99 | p50 ratio | Samples ab / mu | Skipped ab / mu |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 50k-1-patch | sub50k:a-to-b | 22.1 | 24.4 | 27.5 | 430.8 | 507.2 | 625.4 | 19.5× | 2,714 / 2,711 | 0 / 0 |
| 50k-1 | sub50k:a-to-b | 13.6 | 14.2 | 44.1 | 426.5 | 533.9 | 1,927.5 | 31.4× | 2,756 / 2,722 | 0 / 0 |
| 50k-10-fan | sub50k:a-to-b | 17.2 | 29.8 | 89.6 | 4,820.0 | 5,320.0 | 5,602.1 | 280.2× | 2,736 / 2,720 | 0 / 0 |
| 50k-10-patch | sub50k:a-to-b | 22.2 | 27.0 | 34.6 | 703.1 | 2,090.1 | 3,558.5 | 31.7× | 2,751 / 2,718 | 0 / 1 |
| 50k-10 | sub50k:a-to-b | 13.4 | 20.4 | 45.9 | 1,809.8 | 4,185.5 | 5,226.2 | 135.1× | 2,738 / 2,757 | 0 / 0 |
| 50k-100 | sub50k:a-to-b | 28.3 | 54.7 | 80.3 | 2,139.0 | 4,507.1 | 5,706.2 | 75.6× | 2,742 / 2,745 | 0 / 0 |
| 5k-1 | sub5k:a-to-b | 12.1 | 12.6 | 14.0 | 74.1 | 77.3 | 1,666.3 | 6.1× | 2,750 / 2,754 | 0 / 0 |
| 5k-10-fan | sub5k:a-to-b | 16.9 | 23.4 | 33.2 | 3,417.1 | 4,885.0 | 5,081.3 | 202.2× | 2,739 / 2,751 | 0 / 0 |
| 5k-10 | sub5k:a-to-b | 11.6 | 16.6 | 25.2 | 1,235.1 | 3,547.2 | 4,877.5 | 106.5× | 2,765 / 2,773 | 0 / 0 |
| 5k-100 | sub5k:a-to-b | 18.3 | 31.0 | 41.2 | 2,733.2 | 4,948.1 | 5,495.3 | 149.4× | 2,755 / 2,747 | 0 / 0 |
| chromium-1-bidir | chromium:a-to-b | 23.5 | 159.4 | 385.4 | 7,299.5 | 9,908.0 | 11,776.6 | 310.6× | 2,720 / 2,741 | 0 / 0 |
| chromium-1-bidir | chromium:b-to-a | 28.8 | 324.8 | 625.8 | 8,436.9 | 11,144.2 | 13,254.7 | 292.9× | 2,746 / 2,771 | 0 / 0 |
| chromium-1-patch | chromium:a-to-b | 33.1 | 44.4 | 48.2 | 7,575.9 | 9,442.5 | 10,510.8 | 228.9× | 2,708 / 1,823 | 0 / 980 |
| chromium-1 | chromium:a-to-b | 23.8 | 293.0 | 355.8 | 6,232.2 | 8,820.4 | 10,293.3 | 261.9× | 2,730 / 2,741 | 0 / 0 |
| chromium-10-bidir | chromium:a-to-b | 47.1 | 289.9 | 607.8 | 6,062.7 | 8,540.1 | 10,135.9 | 128.7× | 2,738 / 2,753 | 0 / 1 |
| chromium-10-bidir | chromium:b-to-a | 74.6 | 368.0 | 697.9 | 6,329.2 | 8,826.1 | 10,838.9 | 84.8× | 2,713 / 2,732 | 0 / 0 |
| chromium-10-fan-patch | chromium:a-to-b | 42.0 | 67.0 | 107.8 | 11,165.1 | 17,103.1 | 25,404.1 | 265.8× | 2,710 / 1,268 | 0 / 1,576 |
| chromium-10-fan | chromium:a-to-b | 43.9 | 441.4 | 822.2 | 10,881.0 | 12,898.8 | 16,194.5 | 247.9× | 2,723 / 2,713 | 0 / 2 |
| chromium-10-patch | chromium:a-to-b | 34.0 | 44.8 | 58.1 | 7,839.6 | 11,095.3 | 13,304.5 | 230.6× | 2,711 / 1,765 | 0 / 1,038 |
| chromium-10 | chromium:a-to-b | 26.7 | 189.0 | 527.1 | 9,202.4 | 12,160.8 | 13,215.0 | 344.7× | 2,736 / 2,734 | 0 / 0 |
| chromium-100-bidir | chromium:a-to-b | 232.9 | 480.3 | 1,239.4 | 11,331.3 | 15,413.8 | 17,353.7 | 48.7× | 2,731 / 2,720 | 0 / 3 |
| chromium-100-bidir | chromium:b-to-a | 299.0 | 609.9 | 1,305.5 | 12,128.8 | 16,205.0 | 17,873.0 | 40.6× | 2,740 / 2,709 | 0 / 3 |
| chromium-100 | chromium:a-to-b | 80.3 | 339.6 | 681.9 | 8,421.8 | 11,920.3 | 13,463.4 | 104.9× | 2,755 / 2,763 | 0 / 0 |
| two50k-1 | sub50k-b:a-to-b | 12.9 | 20.6 | 35.8 | 367.0 | 447.1 | 1,827.7 | 28.4× | 2,750 / 2,720 | 0 / 0 |
| two50k-1 | sub50k:a-to-b | 13.1 | 14.2 | 44.6 | 444.2 | 558.4 | 1,836.3 | 33.9× | 2,723 / 2,707 | 0 / 0 |
| two50k-10-fan | sub50k-b:a-to-b | 19.3 | 38.7 | 82.7 | 5,128.6 | 5,656.5 | 6,210.2 | 265.7× | 2,733 / 2,730 | 0 / 0 |
| two50k-10-fan | sub50k:a-to-b | 19.0 | 48.3 | 109.0 | 5,019.8 | 5,615.2 | 6,466.1 | 264.2× | 2,736 / 2,725 | 0 / 0 |
| two50k-10 | sub50k-b:a-to-b | 13.2 | 23.8 | 36.7 | 2,040.4 | 4,541.5 | 5,429.4 | 154.6× | 2,720 / 2,741 | 0 / 0 |
| two50k-10 | sub50k:a-to-b | 13.6 | 21.0 | 46.4 | 1,908.8 | 4,375.8 | 5,272.0 | 140.4× | 2,724 / 2,722 | 0 / 0 |
| two50k-100 | sub50k-b:a-to-b | 39.2 | 67.2 | 94.3 | 2,027.4 | 4,387.0 | 5,712.9 | 51.7× | 2,684 / 2,767 | 0 / 0 |
| two50k-100 | sub50k:a-to-b | 31.8 | 62.8 | 90.5 | 2,170.7 | 4,628.4 | 5,786.9 | 68.3× | 2,706 / 2,708 | 0 / 0 |

## Memory and CPU

Median of each run's peak resident memory, in MiB (the aggregate's `/proc` KiB divided by 1024), and mean CPU as a percentage of one core. `local` is the controller/source; `remote` is the destination resource series. These cover the tool process trees, including transport children. Fan-out and two-tree cells have different topology and must not be treated as one-pair per-file memory costs.

### Workload

| Test Cell | Host Endpoint | ab RSS (MiB) | mu RSS (MiB) | ab CPU % | mu CPU % |
| :--- | :--- | ---:| ---:| ---:| ---:|
| 50k-1-patch | local / remote | 69.4 / 52.1 | 268.5 / 194.6 | 2.8 / 1.3 | 87.2 / 60.5 |
| 50k-1 | local / remote | 76.7 / 51.4 | 273.4 / 195.6 | 2.7 / 0.8 | 84.5 / 58.3 |
| 50k-10-fan | local / remote | 565.7 / 404.8 | 2,716.9 / 1,745.1 | 58.8 / 43.3 | 293.2 / 218.0 |
| 50k-10-patch | local / remote | 76.7 / 48.6 | 271.7 / 199.8 | 10.7 / 5.0 | 55.0 / 45.2 |
| 50k-10 | local / remote | 89.6 / 40.9 | 266.3 / 198.6 | 11.6 / 4.1 | 27.1 / 24.9 |
| 50k-100 | local / remote | 107.1 / 50.0 | 277.2 / 206.0 | 36.8 / 17.5 | 48.2 / 46.3 |
| 50k-burst | local / remote | 217.2 / 131.2 | 777.1 / 581.6 | 77.9 / 97.3 | 46.0 / 84.0 |
| 5k-1 | local / remote | 28.4 / 19.7 | 52.7 / 32.3 | 0.6 / 0.2 | 9.5 / 6.9 |
| 5k-10-fan | local / remote | 172.1 / 162.4 | 377.3 / 319.3 | 27.4 / 19.9 | 61.5 / 46.5 |
| 5k-10 | local / remote | 27.6 / 16.5 | 54.0 / 34.5 | 3.3 / 1.3 | 5.1 / 3.8 |
| 5k-100 | local / remote | 33.2 / 22.8 | 52.7 / 38.6 | 14.2 / 8.9 | 14.4 / 9.9 |
| chromium-1-bidir | local / remote | 467.3 / 319.3 | 2,121.8 / 1,377.3 | 21.3 / 28.7 | 155.8 / 138.9 |
| chromium-1-patch | local / remote | 351.1 / 168.9 | 2,078.4 / 1,367.9 | 16.9 / 5.5 | 158.3 / 119.8 |
| chromium-1 | local / remote | 479.0 / 170.4 | 2,081.3 / 1,380.4 | 20.3 / 4.8 | 162.8 / 124.9 |
| chromium-10-bidir | local / remote | 509.7 / 370.1 | 2,128.0 / 1,429.0 | 61.7 / 77.0 | 131.6 / 106.1 |
| chromium-10-fan-patch | local / remote | 2,085.9 / 1,866.8 | 22,441.9 / 12,848.0 | 178.0 / 222.2 | 1,289.0 / 1,091.4 |
| chromium-10-fan | local / remote | 3,449.7 / 1,825.9 | 22,124.6 / 12,861.5 | 278.3 / 185.3 | 1,275.3 / 1,086.5 |
| chromium-10-patch | local / remote | 367.3 / 153.4 | 2,111.0 / 1,427.7 | 50.6 / 19.5 | 110.6 / 123.6 |
| chromium-10 | local / remote | 529.6 / 157.9 | 2,074.4 / 1,472.1 | 60.2 / 19.3 | 91.8 / 112.4 |
| chromium-100-bidir | local / remote | 564.6 / 379.9 | 2,034.5 / 1,398.2 | 95.4 / 71.7 | 113.8 / 87.8 |
| chromium-100 | local / remote | 544.4 / 159.2 | 2,014.3 / 1,451.8 | 94.2 / 37.9 | 105.2 / 123.9 |
| chromium-burst | local / remote | 385.3 / 216.3 | 1,914.6 / 1,316.8 | 11.5 / 12.2 | 113.2 / 93.1 |
| two50k-1 | local / remote | 119.6 / 66.0 | 448.9 / 320.8 | 4.8 / 1.4 | 159.8 / 106.5 |
| two50k-10-fan | local / remote | 838.1 / 662.4 | 4,830.9 / 3,140.0 | 134.7 / 81.9 | 550.0 / 386.8 |
| two50k-10 | local / remote | 145.2 / 77.2 | 405.2 / 352.2 | 22.7 / 8.8 | 46.0 / 48.7 |
| two50k-100 | local / remote | 187.0 / 86.1 | 446.6 / 362.0 | 86.9 / 37.4 | 91.5 / 91.5 |

### Idle

During idle state on a 505k Chromium repository:
- **Autobahn Controller:** 386.5 MiB RSS, **0.1% CPU**.
- **Mutagen Controller:** 1,713.3 MiB RSS, **49.9% CPU**.

| Test Cell | Host Endpoint | ab RSS (MiB) | mu RSS (MiB) | ab CPU % | mu CPU % |
| :--- | :--- | ---:| ---:| ---:| ---:|
| 50k-1-patch | local / remote | 67.9 / 43.3 | 229.3 / 159.5 | 0.1 / 0.0 | 5.7 / 5.5 |
| 50k-1 | local / remote | 67.1 / 43.3 | 214.8 / 163.7 | 0.1 / 0.0 | 5.8 / 5.6 |
| 50k-10-fan | local / remote | 516.2 / 398.5 | 1,824.4 / 1,591.0 | 0.7 / 0.2 | 56.2 / 59.0 |
| 50k-10-patch | local / remote | 68.3 / 43.1 | 223.0 / 157.3 | 0.1 / 0.0 | 5.7 / 5.7 |
| 50k-10 | local / remote | 67.5 / 42.7 | 219.5 / 156.2 | 0.1 / 0.0 | 5.8 / 5.6 |
| 50k-100 | local / remote | 68.2 / 42.7 | 216.3 / 160.4 | 0.1 / 0.0 | 5.7 / 5.8 |
| 50k-burst | local / remote | 67.8 / 42.8 | 211.4 / 163.6 | 0.1 / 0.0 | 5.6 / 5.9 |
| 5k-1 | local / remote | 23.1 / 11.5 | 47.8 / 29.7 | 0.1 / 0.0 | 0.6 / 0.6 |
| 5k-10-fan | local / remote | 164.8 / 114.8 | 289.8 / 287.2 | 0.8 / 0.3 | 5.6 / 6.7 |
| 5k-10 | local / remote | 23.4 / 11.6 | 47.5 / 29.4 | 0.1 / 0.0 | 0.6 / 0.5 |
| 5k-100 | local / remote | 22.9 / 11.4 | 47.2 / 29.6 | 0.1 / 0.0 | 0.6 / 0.5 |
| chromium-1-bidir | local / remote | 387.1 / 169.7 | 1,692.5 / 1,276.2 | 0.1 / 0.0 | 48.8 / 51.4 |
| chromium-1-patch | local / remote | 386.6 / 168.9 | 1,793.5 / 1,285.2 | 0.1 / 0.0 | 49.2 / 49.3 |
| chromium-1 | local / remote | 386.5 / 170.4 | 1,713.3 / 1,268.8 | 0.1 / 0.0 | 49.9 / 51.1 |
| chromium-10-bidir | local / remote | 386.7 / 170.5 | 1,728.7 / 1,231.3 | 0.1 / 0.0 | 50.0 / 50.3 |
| chromium-10-fan-patch | local / remote | 3,037.0 / 1,923.8 | 15,094.6 / 12,428.9 | 0.8 / 0.3 | 565.0 / 521.2 |
| chromium-10-fan | local / remote | 3,055.3 / 1,891.4 | 15,149.4 / 12,560.3 | 0.8 / 0.3 | 611.8 / 515.6 |
| chromium-10-patch | local / remote | 387.3 / 170.9 | 1,654.1 / 1,241.7 | 0.1 / 0.0 | 49.1 / 49.5 |
| chromium-10 | local / remote | 386.7 / 170.8 | 1,714.2 / 1,271.1 | 0.1 / 0.0 | 48.0 / 50.8 |
| chromium-100-bidir | local / remote | 387.0 / 170.6 | 1,730.7 / 1,278.0 | 0.1 / 0.0 | 47.5 / 50.4 |
| chromium-100 | local / remote | 386.0 / 170.2 | 1,718.3 / 1,261.1 | 0.1 / 0.0 | 48.1 / 50.5 |
| chromium-burst | local / remote | 386.3 / 170.2 | 1,751.2 / 1,277.3 | 0.1 / 0.0 | 48.7 / 49.8 |
| coldsync-50k-fan | local / remote | 500.0 / 859.6 | 1,834.5 / 1,681.5 | 0.7 / 0.2 | 57.2 / 58.0 |
| coldsync-50k | local / remote | 71.3 / 87.8 | 237.8 / 172.0 | 0.1 / 0.0 | 5.7 / 5.9 |
| coldsync-5k-fan | local / remote | 209.0 / 305.6 | 304.3 / 317.3 | 0.8 / 0.2 | 5.5 / 6.4 |
| coldsync-5k | local / remote | 29.2 / 36.6 | 49.7 / 31.9 | 0.1 / 0.0 | 0.6 / 0.6 |
| coldsync-chromium-fan | local / remote | 1,210.8 / 2,704.0 | 15,190.6 / 12,758.1 | 0.7 / 46.9 | 563.6 / 511.8 |
| coldsync-chromium | local / remote | 210.2 / 271.4 | 1,812.1 / 1,345.9 | 0.1 / 5.0 | 48.9 / 50.3 |
| two50k-1 | local / remote | 116.4 / 64.4 | 354.2 / 291.5 | 0.2 / 0.0 | 9.9 / 9.9 |
| two50k-10-fan | local / remote | 867.6 / 671.0 | 3,240.5 / 2,956.3 | 1.4 / 0.5 | 112.0 / 105.1 |
| two50k-10 | local / remote | 115.0 / 70.8 | 343.9 / 284.5 | 0.2 / 0.0 | 9.9 / 10.6 |
| two50k-100 | local / remote | 115.4 / 64.4 | 349.7 / 288.6 | 0.2 / 0.0 | 10.0 / 10.2 |

## First synchronization

Dedicated `coldsync-*` cells start with empty destinations. Values are median digest-verified seconds across five repeats, including verification and completion polling.

Ordinary latency cells are seeded and do not measure first-sync throughput.

| Cell | Autobahn seconds | mutagen seconds |
|---|---:|---:|
| coldsync-50k-fan | 23.9 | 57.8 |
| coldsync-50k | 21.9 | 52.1 |
| coldsync-5k-fan | 7.7 | 23.0 |
| coldsync-5k | 4.9 | 7.3 |
| coldsync-chromium-fan | 221.3 | 505.4 |
| coldsync-chromium | 228.1 | 454.8 |

## Bursts

These values are median wall seconds for repeated module copies, including convergence verification. Internal Autobahn cycle duration is a separate metric retained in the aggregate.

| Cell | Autobahn seconds | mutagen seconds |
|---|---:|---:|
| 50k-burst | 3.5 | 7.5 |
| chromium-burst | 4.5 | 6.9 |
