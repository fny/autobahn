# Benchmark matrix: September–October 2026

This is the latest recorded matrix, refreshed through October 1. It is a composite of complete cells from identified builds, not a run of today's HEAD. The [summary](./benchmarks.md) explains the headline results. The committed [aggregate](../benchmarks/2026-10-01.json) contains the figures, sample counts, per-run ranges, exclusions, and problem records used below.

## Provenance

| Cells | Autobahn build | Run | Date |
|---|---|---|---|
| All except the five patch cells below | `e7b3ac0`, 0.4.0+e16, musl `dist` with mimalloc | `bench-1790601586` | 2026-09-28 |
| `50k-1-patch`, `50k-10-patch` | `98598b8` | `bench-1790773610` | 2026-09-30 |
| `chromium-10-fan-patch` | `0c72865` | `bench-1790869509` | 2026-10-01 |
| `chromium-1-patch`, `chromium-10-patch` | `0c72865` | `bench-1790873270` | 2026-10-01 |

Each replacement carries both tools' measurements from the same follow-up run. Intermediate patch runs (`1790773610`'s Chromium cells and `1790826510`) are superseded here. The base run's Autobahn binary SHA-256 is `5dfaffcf415724c2e1c0440c6610cb482d43d8fdb7c3def68c7552e474f6ce76`; its harness SHA-256 is `a46f791514a2b4a6ce964af6c2e492b138a1a99c36c730a5e7a2a73331055f04`.

The base matrix has 32 cells × 5 repeats: 160 completed jobs, each running Autobahn and mutagen 0.19.0-dev on the same machines in randomized order. There are no excluded tool-runs, censored latency samples, or inconsistent resource series in the published aggregate. Follow-up runs also completed with five repeats per cell and no exclusions or censored samples. Skipped workload ticks remain reported; they are an offered-load shortfall, not a timeout.

The base run used 26 groups on 151 Linux instances in us-east-2: 106 `c6i.4xlarge`, 30 `c6i.2xlarge`, and 15 `c6i.xlarge`. Smaller cells can run on larger groups. The image was `ami-096a78f9bffc18f94`, with Chromium at `87d2dbeceeb6ee28744922bdcf7be799c065627e`. Corpus labels `5k` and `50k` name approximate subsets; they are not the older report's `4k` and `40k` cells. The full checkout is about 505,000 files. Harness floor p50 was 0.7 ms (0.5–0.8 ms across the base run).

Raw JSONL and plans remain in the local `bench/results-bench-<run>/` directories; these directories are normally ignored by Git. The committed aggregate makes the tables available in a clean checkout, but is not a substitute for raw samples when re-running the analysis. See [bench/README.md](../bench/README.md) for the harness.

## Propagation latency

All percentiles are pooled milliseconds across five repeats. `ab` is Autobahn, `mu` is mutagen. Samples and skipped ticks are shown as `ab / mu`. `-bidir` measures each direction separately; `-fan` has ten destinations; `-patch` changes ranges within large files instead of replacing small files. The direction labels retain the corpus identity for two-tree cells.

| Cell | Direction | ab p50 | ab p90 | ab p99 | mu p50 | mu p90 | mu p99 | p50 ratio | Samples ab / mu | Skipped ab / mu |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 50k-1-patch | sub50k:a-to-b | 22.7 | 25.2 | 29.7 | 447.4 | 546.5 | 686.3 | 19.7× | 2,748 / 2,729 | 0 / 0 |
| 50k-1 | sub50k:a-to-b | 13.6 | 14.2 | 44.1 | 426.5 | 533.9 | 1,927.5 | 31.4× | 2,756 / 2,722 | 0 / 0 |
| 50k-10-fan | sub50k:a-to-b | 17.2 | 29.8 | 89.6 | 4,820.0 | 5,320.0 | 5,602.1 | 280.2× | 2,736 / 2,720 | 0 / 0 |
| 50k-10-patch | sub50k:a-to-b | 22.7 | 27.5 | 35.5 | 709.1 | 2,065.6 | 3,509.4 | 31.2× | 2,728 / 2,703 | 0 / 1 |
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
| chromium-10-fan-patch | chromium:a-to-b | 42.0 | 67.0 | 107.8 | 11,165.1 | 17,103.1 | 25,404.1 | 265.8× | 2,710 / 1,268 | 0 / 1576 |
| chromium-10-fan | chromium:a-to-b | 43.9 | 441.4 | 822.2 | 10,881.0 | 12,898.8 | 16,194.5 | 247.9× | 2,723 / 2,713 | 0 / 2 |
| chromium-10-patch | chromium:a-to-b | 34.0 | 44.8 | 58.1 | 7,839.6 | 11,095.3 | 13,304.5 | 230.6× | 2,711 / 1,765 | 0 / 1038 |
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

| Cell | Host | ab MiB | mu MiB | ab CPU % | mu CPU % |
|---|---|---:|---:|---:|---:|
| 50k-1-patch | local | 73.9 | 269.7 | 2.9 | 91.9 |
| 50k-1-patch | remote | 49.4 | 184.0 | 1.4 | 62.4 |
| 50k-1 | local | 76.7 | 273.4 | 2.7 | 84.5 |
| 50k-1 | remote | 51.4 | 195.6 | 0.8 | 58.3 |
| 50k-10-fan | local | 565.7 | 2,716.9 | 58.8 | 293.2 |
| 50k-10-fan | remote | 404.8 | 1,745.1 | 43.3 | 218.0 |
| 50k-10-patch | local | 75.7 | 273.8 | 11.0 | 56.6 |
| 50k-10-patch | remote | 35.9 | 199.3 | 5.9 | 48.6 |
| 50k-10 | local | 89.6 | 266.3 | 11.6 | 27.1 |
| 50k-10 | remote | 40.9 | 198.6 | 4.1 | 24.9 |
| 50k-100 | local | 107.1 | 277.2 | 36.8 | 48.2 |
| 50k-100 | remote | 50.0 | 206.0 | 17.5 | 46.3 |
| 50k-burst | local | 217.2 | 777.1 | 77.9 | 46.0 |
| 50k-burst | remote | 131.2 | 581.6 | 97.3 | 84.0 |
| 5k-1 | local | 28.4 | 52.7 | 0.6 | 9.5 |
| 5k-1 | remote | 19.7 | 32.3 | 0.2 | 6.9 |
| 5k-10-fan | local | 172.1 | 377.3 | 27.4 | 61.5 |
| 5k-10-fan | remote | 162.4 | 319.3 | 19.9 | 46.5 |
| 5k-10 | local | 27.6 | 54.0 | 3.3 | 5.1 |
| 5k-10 | remote | 16.5 | 34.5 | 1.3 | 3.8 |
| 5k-100 | local | 33.2 | 52.7 | 14.2 | 14.4 |
| 5k-100 | remote | 22.8 | 38.6 | 8.9 | 9.9 |
| chromium-1-bidir | local | 467.3 | 2,121.8 | 21.3 | 155.8 |
| chromium-1-bidir | remote | 319.3 | 1,377.3 | 28.7 | 138.9 |
| chromium-1-patch | local | 351.1 | 2,078.4 | 16.9 | 158.3 |
| chromium-1-patch | remote | 168.9 | 1,367.9 | 5.5 | 119.8 |
| chromium-1 | local | 479.0 | 2,081.3 | 20.3 | 162.8 |
| chromium-1 | remote | 170.4 | 1,380.4 | 4.8 | 124.9 |
| chromium-10-bidir | local | 509.7 | 2,128.0 | 61.7 | 131.6 |
| chromium-10-bidir | remote | 370.1 | 1,429.0 | 77.0 | 106.1 |
| chromium-10-fan-patch | local | 2,085.9 | 22,441.9 | 178.0 | 1289.0 |
| chromium-10-fan-patch | remote | 1,866.8 | 12,848.0 | 222.2 | 1091.4 |
| chromium-10-fan | local | 3,449.7 | 22,124.6 | 278.3 | 1275.3 |
| chromium-10-fan | remote | 1,825.9 | 12,861.5 | 185.3 | 1086.5 |
| chromium-10-patch | local | 367.3 | 2,111.0 | 50.6 | 110.6 |
| chromium-10-patch | remote | 153.4 | 1,427.7 | 19.5 | 123.6 |
| chromium-10 | local | 529.6 | 2,074.4 | 60.2 | 91.8 |
| chromium-10 | remote | 157.9 | 1,472.1 | 19.3 | 112.4 |
| chromium-100-bidir | local | 564.6 | 2,034.5 | 95.4 | 113.8 |
| chromium-100-bidir | remote | 379.9 | 1,398.2 | 71.7 | 87.8 |
| chromium-100 | local | 544.4 | 2,014.3 | 94.2 | 105.2 |
| chromium-100 | remote | 159.2 | 1,451.8 | 37.9 | 123.9 |
| chromium-burst | local | 385.3 | 1,914.6 | 11.5 | 113.2 |
| chromium-burst | remote | 216.3 | 1,316.8 | 12.2 | 93.1 |
| two50k-1 | local | 119.6 | 448.9 | 4.8 | 159.8 |
| two50k-1 | remote | 66.0 | 320.8 | 1.4 | 106.5 |
| two50k-10-fan | local | 838.1 | 4,830.9 | 134.7 | 550.0 |
| two50k-10-fan | remote | 662.4 | 3,140.0 | 81.9 | 386.8 |
| two50k-10 | local | 145.2 | 405.2 | 22.7 | 46.0 |
| two50k-10 | remote | 77.2 | 352.2 | 8.8 | 48.7 |
| two50k-100 | local | 187.0 | 446.6 | 86.9 | 91.5 |
| two50k-100 | remote | 86.1 | 362.0 | 37.4 | 91.5 |

### Idle

| Cell | Host | ab MiB | mu MiB | ab CPU % | mu CPU % |
|---|---|---:|---:|---:|---:|
| 50k-1-patch | local | 67.9 | 220.3 | 0.1 | 5.7 |
| 50k-1-patch | remote | 43.0 | 160.2 | 0.0 | 5.8 |
| 50k-1 | local | 67.1 | 214.8 | 0.1 | 5.8 |
| 50k-1 | remote | 43.3 | 163.7 | 0.0 | 5.6 |
| 50k-10-fan | local | 516.2 | 1,824.4 | 0.7 | 56.2 |
| 50k-10-fan | remote | 398.5 | 1,591.0 | 0.2 | 59.0 |
| 50k-10-patch | local | 68.8 | 223.1 | 0.1 | 5.5 |
| 50k-10-patch | remote | 42.9 | 160.8 | 0.0 | 6.0 |
| 50k-10 | local | 67.5 | 219.5 | 0.1 | 5.8 |
| 50k-10 | remote | 42.7 | 156.2 | 0.0 | 5.6 |
| 50k-100 | local | 68.2 | 216.3 | 0.1 | 5.7 |
| 50k-100 | remote | 42.7 | 160.4 | 0.0 | 5.8 |
| 50k-burst | local | 67.8 | 211.4 | 0.1 | 5.6 |
| 50k-burst | remote | 42.8 | 163.6 | 0.0 | 5.9 |
| 5k-1 | local | 23.1 | 47.8 | 0.1 | 0.6 |
| 5k-1 | remote | 11.5 | 29.7 | 0.0 | 0.6 |
| 5k-10-fan | local | 164.8 | 289.8 | 0.8 | 5.6 |
| 5k-10-fan | remote | 114.8 | 287.2 | 0.3 | 6.7 |
| 5k-10 | local | 23.4 | 47.5 | 0.1 | 0.6 |
| 5k-10 | remote | 11.6 | 29.4 | 0.0 | 0.5 |
| 5k-100 | local | 22.9 | 47.2 | 0.1 | 0.6 |
| 5k-100 | remote | 11.4 | 29.6 | 0.0 | 0.5 |
| chromium-1-bidir | local | 387.1 | 1,692.5 | 0.1 | 48.8 |
| chromium-1-bidir | remote | 169.7 | 1,276.2 | 0.0 | 51.4 |
| chromium-1-patch | local | 386.6 | 1,793.5 | 0.1 | 49.2 |
| chromium-1-patch | remote | 168.9 | 1,285.2 | 0.0 | 49.3 |
| chromium-1 | local | 386.5 | 1,713.3 | 0.1 | 49.9 |
| chromium-1 | remote | 170.4 | 1,268.8 | 0.0 | 51.1 |
| chromium-10-bidir | local | 386.7 | 1,728.7 | 0.1 | 50.0 |
| chromium-10-bidir | remote | 170.5 | 1,231.3 | 0.0 | 50.3 |
| chromium-10-fan-patch | local | 3,037.0 | 15,094.6 | 0.8 | 565.0 |
| chromium-10-fan-patch | remote | 1,923.8 | 12,428.9 | 0.3 | 521.2 |
| chromium-10-fan | local | 3,055.3 | 15,149.4 | 0.8 | 611.8 |
| chromium-10-fan | remote | 1,891.4 | 12,560.3 | 0.3 | 515.6 |
| chromium-10-patch | local | 387.3 | 1,654.1 | 0.1 | 49.1 |
| chromium-10-patch | remote | 170.9 | 1,241.7 | 0.0 | 49.5 |
| chromium-10 | local | 386.7 | 1,714.2 | 0.1 | 48.0 |
| chromium-10 | remote | 170.8 | 1,271.1 | 0.0 | 50.8 |
| chromium-100-bidir | local | 387.0 | 1,730.7 | 0.1 | 47.5 |
| chromium-100-bidir | remote | 170.6 | 1,278.0 | 0.0 | 50.4 |
| chromium-100 | local | 386.0 | 1,718.3 | 0.1 | 48.1 |
| chromium-100 | remote | 170.2 | 1,261.1 | 0.0 | 50.5 |
| chromium-burst | local | 386.3 | 1,751.2 | 0.1 | 48.7 |
| chromium-burst | remote | 170.2 | 1,277.3 | 0.0 | 49.8 |
| coldsync-50k-fan | local | 500.0 | 1,834.5 | 0.7 | 57.2 |
| coldsync-50k-fan | remote | 859.6 | 1,681.5 | 0.2 | 58.0 |
| coldsync-50k | local | 71.3 | 237.8 | 0.1 | 5.7 |
| coldsync-50k | remote | 87.8 | 172.0 | 0.0 | 5.9 |
| coldsync-5k-fan | local | 209.0 | 304.3 | 0.8 | 5.5 |
| coldsync-5k-fan | remote | 305.6 | 317.3 | 0.2 | 6.4 |
| coldsync-5k | local | 29.2 | 49.7 | 0.1 | 0.6 |
| coldsync-5k | remote | 36.6 | 31.9 | 0.0 | 0.6 |
| coldsync-chromium-fan | local | 1,210.8 | 15,190.6 | 0.7 | 563.6 |
| coldsync-chromium-fan | remote | 2,704.0 | 12,758.1 | 46.9 | 511.8 |
| coldsync-chromium | local | 210.2 | 1,812.1 | 0.1 | 48.9 |
| coldsync-chromium | remote | 271.4 | 1,345.9 | 5.0 | 50.3 |
| two50k-1 | local | 116.4 | 354.2 | 0.2 | 9.9 |
| two50k-1 | remote | 64.4 | 291.5 | 0.0 | 9.9 |
| two50k-10-fan | local | 867.6 | 3,240.5 | 1.4 | 112.0 |
| two50k-10-fan | remote | 671.0 | 2,956.3 | 0.5 | 105.1 |
| two50k-10 | local | 115.0 | 343.9 | 0.2 | 9.9 |
| two50k-10 | remote | 70.8 | 284.5 | 0.0 | 10.6 |
| two50k-100 | local | 115.4 | 349.7 | 0.2 | 10.0 |
| two50k-100 | remote | 64.4 | 288.6 | 0.0 | 10.2 |

## First synchronization

Dedicated unseeded cells, median digest-verified seconds across five repeats. Ordinary latency cells are pre-seeded and do not measure first-sync throughput. Verification and completion polling are included.

| Cell | Autobahn seconds | mutagen seconds |
|---|---:|---:|
| coldsync-50k-fan | 23.9 | 57.8 |
| coldsync-50k | 21.9 | 52.1 |
| coldsync-5k-fan | 7.7 | 23.0 |
| coldsync-5k | 4.9 | 7.3 |
| coldsync-chromium-fan | 221.3 | 505.4 |
| coldsync-chromium | 228.1 | 454.8 |

## Bursts

Median wall seconds for repeated module copies, as reported by the aggregate. These are convergence measurements, including the harness's verification work; Autobahn's internal cycle timing is a different metric and is retained in the aggregate.

| Cell | Autobahn seconds | mutagen seconds |
|---|---:|---:|
| 50k-burst | 3.5 | 7.5 |
| chromium-burst | 4.5 | 6.9 |

The [0.3.0 matrix](./benchmark-matrix-0.3.md) is retained separately. Its corpus, build, and method differ; compare tools within a run rather than attributing every difference between reports to a code change.
