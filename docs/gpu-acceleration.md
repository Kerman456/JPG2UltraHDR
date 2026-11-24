# 转换过程能否用 GPU 加速

结论先说：**不值得做**。瓶颈确实存在，但它不在 GPU 擅长的位置，而且两条输出路径各有硬性障碍。
下面是本机实测数据与推理，机器为 i7-13700HX（16 核 / 24 线程）+ RTX 4060 Laptop 8 GB，
源图 `DSC_0227.jpg`（5568×3712，20.7 MP）。

## 一、时间花在哪

同一张图，`cargo run --release --bin profile`：

| 阶段 | JPG | HEIC |
|---|---|---|
| 读取 | 0.8 ms | 0.8 ms |
| 解码 JPEG | 60.5 ms（4.8%） | 61.1 ms（0.4%） |
| 自动推断曲线 | 17 ms | 17 ms |
| 生成增益图 | 42.8 ms（3.4%） | 25.3 ms（0.2%） |
| **编码 + 写盘** | **1146.7 ms（90.4%）** | **13927.7 ms（99.3%）** |
| 合计 | 1267.8 ms | 14032.2 ms |

像素级的工作（解码、算增益图）加起来不到 5%。**要加速只能加速编码器**，
而编码器是熵编码，天生串行依赖强，不是 GPU 的强项。

再拆细一点（`bench_encode_split`，三次取最快）：

| 组合 | 耗时 |
|---|---|
| 主图 only（基线 JPEG） | 720 ms |
| 主图 only（渐进式 JPEG） | 1015 ms |
| 主图 + 增益图（基线） | 867 ms |
| 主图 + 增益图（渐进式，当前实际路径） | 1143 ms |

增益图只占 146 ms（17%），**主图占 87%**。

## 二、JPG 路径的障碍

### 1. 编码器占用不到一个核，但这不是 GPU 的机会

`bench_encode_split` 运行期间采样进程 CPU 时间：峰值 **1.08 核**（24 线程中的 1 个）。
`mozjpeg-rs` 的编码是单线程的 —— 这正是批量转换能靠并发扩展的原因（`bench_concurrency`，
同一张图重复转换，单位为张/秒）：

| 并发 worker | 吞吐（多次运行的范围） |
|---|---|
| 1 | 0.52 – 0.67 |
| 2 | 0.50 – 0.54 |
| 4 | 0.92 – 0.96 |
| 8 | 1.63 – 3.03 |
| 16 | 1.72 – 1.94 |

8 并发时峰值 9.3 核，16 并发时 16.8 核。**8 到 16 之间吞吐已经进入平台期**
（多次测量互有胜负，方差本身就有 1.6–3.0 那么大），说明加并发到这个量级就不再是
「核不够」而是「内存带宽和分配器在打架」。所以：**单张慢是因为熵编码串行，
不是因为算力不够**。GPU 能改善的只有单张延迟，而批量场景本来就已经把 CPU 用到 40–70%，
没有空出来的算力可以「白拿」。

### 2. nvJPEG 的 GPU 编码器不支持渐进式

这是决定性的一条。默认设置 `progressive: true`，而 nvJPEG 的编码器
`nvjpegEncoderParamsSetEncoding` 传 `NVJPEG_ENCODING_PROGRESSIVE_DCT_HUFFMAN`
会返回参数错误（[NVIDIA/CUDALibrarySamples#189](https://github.com/NVIDIA/CUDALibrarySamples/issues/189)
里有人报了同样的 `#2`），只有 `NVJPEG_ENCODING_BASELINE_DCT` 能用。
硬件 JPEG 编码单元（`NVJPG`）在 NVIDIA 的
[open-gpu-doc](https://nvidia.github.io/open-gpu-doc/classes/video/nvjpg_drv.h)
里也写明只支持 baseline sequential。

改走 GPU 就得放弃渐进式，代价是实测文件**反而变大 2403 字节**
（3824774 → 3827177），还丢掉渐进式「先糊后清」的加载体验。这是净损失。

### 3. 生态没有可用的 Rust 绑定

crates.io 实查：`nvjpeg-sys` 只有 `0.1.0`（2024-03 之后没再动，2662 次下载，没有仓库地址），
`nvidia-video-codec-sdk` `0.4.0` 只是 NVENC 的裸绑定。本机也没装 CUDA Toolkit
（`CUDA_PATH` 为空、没有 `nvcc`、磁盘上没有 `nvjpeg64_*.dll` —— nvJPEG 随 Toolkit 分发，
不随驱动分发）。要为它引入 CUDA 构建链、把单文件绿色程序变成「必须装驱动 SDK」，
换来的只是单张 1.2 s → 可能 0.4 s。

## 三、HEIC 路径的障碍

HEIC 更没戏，但原因不同：它已经**用满了多核**。

`heifer` 按 512×512 分块，`EncodeOptions::max_threads = 0` 表示用满全部核。
实测单张 HEIC 编码的线程缩放（`bench_heifer_threads`）：

| max_threads | 单张耗时 |
|---|---|
| 0（全部 24 核） | 8 798 ms |
| 4 | 21 765 ms |
| 2 | 45 897 ms |
| 1 | 95 141 ms |

1 → 24 线程提速 **10.8 倍**，说明它确实在并行。8 个并发时峰值 13.6 核（57%），
但单张耗时几乎不降（14.26 s → 12.69 s，只有 11%）：**24 个核已经喂不饱一个 20 MP 的 HEVC 编码**，
再加并发只是在同一批核上排队。这恰恰是 GPU 理论上该赢的形状（算力不够 → 换更快的算力），
但现实有三道槛：

1. **NVENC 只出视频码流**。heifer 需要的是 HEIF 容器里的分块 HEVC 静帧 + 独立的单色增益图平面，
   NVENC 给的是单条视频码流，分块、mux、增益图关联都得自己拿 `heifer-isobmff` 拼。
2. **同码率下质量更差**。NVENC 的 HEVC 在同码率下比 x265 差约一半
   （[对比](https://www.technolynx.com/post/hevc-encoders-compared-x265-nvenc-and-quick-sync-for-streaming/)），
   而 HEIC 输出本来就是「用画质换体积」，再降一档就失去意义。
3. **静帧用不到硬件的优势**。NVENC 的价值在实时吞吐，单张 20 MP 静帧的瓶颈是熵编码的串行性，
   换到 GPU 上仍然要串行做 CABAC。

## 四、真正的优化空间（都在 CPU 上，不需要 GPU）

按性价比排序：

1. **让主图和增益图并行编码**。现在 `ultrajpeg::Encoder::encode` 是先编主图、再编增益图
   （`lib.rs:363-390` 顺序调用 `encode_image`），而两者互不依赖。主图 720 ms + 增益图 146 ms
   串行 = 866 ms，并行后应降到 ~730 ms，**约 16%**。这条最容易做，只要把主图那条路径
   换成「自己调 `encode_image` 两次 + `assemble_container_owned`」——但
   `assemble_container_owned` 是 `pub(crate)`，需要给上游提 issue 或 fork。
2. **先修 `concurrency()` 的抖动，再谈提上限**。现在是
   `min(核数, (内存-2GB)/200MB, 8)`，上限硬编码 8。实测 8 并发的吞吐在 1.63–3.03 张/秒
   之间来回跳，同一份代码重复跑两次能差一倍 —— 说明提上限之前，先要排除分配器和
   页面缓存争用（比如给每个 worker 预分配缓冲、复用 `RawImage` 的 Vec）。
   在抖动没解决前把上限从 8 提到 16，只会把方差放大，不保证更快。
3. **渐进式只在需要时开**。渐进式比基线慢 295 ms（1015 vs 720），体积只小 2403 字节（0.06%）。
   对大图来说这个交换不太划算，可以考虑超过某个像素数就自动关掉。
4. **HEIC 降 quality**。quality 80 的 20 MP 约 2.9 MB / 8.8 s，quality 70 约 2.05 MB / 7.7 s。
   如果接受更小体积，这是唯一直接的提速手段。

## 五、一句话总结

- **JPG**：单张 1.2 s 里 90% 是单线程熵编码。GPU 的 JPEG 编码器不支持渐进式（会变大且丢体验），
  Rust 绑定也不可用。批量靠并发已经吃到 40–70% CPU，再往上先要解决吞吐抖动。
- **HEIC**：编码占 99%，已经用满 24 核且 10.8 倍并行，属于「算力不够」而非「没并行」，
  是唯一理论上该上 GPU 的场景；但 NVENC 出不了 HEIF 静帧容器、质量还更差。
- 想快，先做第四节那几条 CPU 优化，收益确定且不引入任何依赖。
