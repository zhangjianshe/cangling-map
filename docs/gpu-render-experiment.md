# GPU 后台渲染实验

本分支用于验证 GPU 是否适合承担 Cangling Map 的矢量栅格化阶段，不改变生产渲染器。

## 基准边界

`gpu-render-bench` 使用同一批半透明矩形，对比：

- CPU：当前生产依赖 `tiny-skia` 的抗锯齿路径填充；
- GPU：`wgpu` 离屏 render pass，包含顶点上传、提交、等待和 RGBA 回读。

这里有意包含 GPU 回读，因为 WMS/WMTS 最终仍需在 CPU 侧执行 PNG/JPEG 编码。基准不包含
GDAL 查询、投影、标签布局、字体塑形和图片编码，因此只是评估栅格化后端的上限，不能替代
真实中国行政区图层的端到端压测。

## 运行

```bash
cargo run --release -p gpu-render-bench
GPU_BENCH_ITERATIONS=30 cargo run --release -p gpu-render-bench
GPU_BENCH_ADAPTER=NVIDIA cargo run --release -p gpu-render-bench
```

输出为 CSV：图像尺寸、图元数量、CPU 耗时、GPU 总耗时和加速比。加速比大于 1 表示 GPU
更快。应分别记录集显、独显和无 GPU 容器的结果。

## 是否进入生产的判断条件

- 256×256 瓦片必须在包含上传和回读后仍有稳定收益；
- 并发压测的 P95 和吞吐量优于 CPU，而不是只有单请求平均值更好；
- GPU 故障时能自动降级到 CPU；
- 文字、halo、虚线、图案填充和抗锯齿输出与 CPU 后端可接受地一致；
- 容器能够稳定访问 `/dev/dri` 或 NVIDIA runtime。

如果小瓦片没有收益，更现实的落点是使用 GPU 处理大幅 WMS 合成、批量瓦片或预热任务，
而继续用 CPU 渲染普通 256/512 像素 WMTS 请求。

## 2026-10-21 首轮结果

测试设备为 Intel Raptor Lake-S UHD Graphics，后端为 Vulkan，release 模式，每组预热一次后
执行 10 次。GPU 总耗时包含顶点上传、离屏绘制、等待和 RGBA 回读。

| 尺寸 | 图元数 | tiny-skia CPU | GPU 总耗时 | 加速比 |
| --- | ---: | ---: | ---: | ---: |
| 256×256 | 250 | 0.828 ms | 7.695 ms | 0.11× |
| 1024×1024 | 2,000 | 51.174 ms | 11.790 ms | 4.34× |
| 2048×2048 | 8,000 | 743.492 ms | 80.659 ms | 9.22× |

结论：普通 256×256 单瓦片不适合逐请求提交 GPU；大幅 WMS 或合并批处理有明显潜力。下一轮
应使用真实行政区多边形，拆分 GDAL 查询、几何整理、GPU draw、readback 和 PNG 编码，并进行
多请求吞吐量与 P95 对比。

主机能够枚举 RTX 4060 PCI 设备，但当前 NVIDIA NVML 报告驱动/库版本不匹配，Vulkan 侧也
没有向 `wgpu` 暴露 NVIDIA adapter，因此本轮独显测试未执行。修复宿主机驱动后可使用
`GPU_BENCH_ADAPTER=NVIDIA` 重跑。

## 2026-10-21 RTX 4060 复测

重启主机后，NVIDIA 内核模块与用户态库统一为 `595.99.02`，`wgpu` 通过 Vulkan 正常选择
NVIDIA GeForce RTX 4060 Laptop GPU。release 模式、每组 10 次的结果如下：

| 尺寸 | 图元数 | tiny-skia CPU | RTX 4060 总耗时 | 加速比 |
| --- | ---: | ---: | ---: | ---: |
| 256×256 | 250 | 0.858 ms | 1.104 ms | 0.78× |
| 1024×1024 | 2,000 | 52.028 ms | 2.479 ms | 20.98× |
| 2048×2048 | 8,000 | 744.972 ms | 16.320 ms | 45.65× |

RTX 4060 大幅降低了固定提交与回读成本，但 256×256 单瓦片仍略慢于 CPU。结论保持不变：
普通小瓦片应继续走 CPU 或合并成批次提交；大幅 WMS、批量瓦片和缓存预热是 GPU 后端的
优先应用场景。
