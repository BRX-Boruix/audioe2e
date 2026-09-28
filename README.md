# audioe2e

BORUIX 的音频端到端验收程序：经真实系统调用完成一次阻塞唤醒的写入与读回。

[English](README.en.md)

## 测什么

以两个进程协作完成一轮往返：

- 写入端附加音频消费者槽位，写入一帧已知 PCM 后退出
- 读取端附加同一槽位，阻塞等待数据，取回后逐字节核对，确认消费后释放槽位

它覆盖内核启动期自测触不到的两段路径：音频系统调用封装本身（参数打包、错误码翻译、用户缓冲
校验），以及要求真实进程上下文切换的阻塞唤醒往返。

## 用法

命令行字为 `producer` 或 `consumer`，两端成对运行，通常由 [`selftest`](https://github.com/BRX-Boruix/selftest) 协调：

```
[audioe2e] producer wrote full frame via VFS path
[audioe2e] consumer fetched full frame
[audioe2e] consumer verified payload byte-for-byte
[audioe2e] PASS: blocked reader woke, verified, committed
```

## 退出码

- `0`——该端完成且全部核对通过
- 非零——失败；附加被拒、写入被拒、等待超限、数据不符各不相同，输出行注明

## 构建

```bash
cargo build --release
```

## 文件结构

```
audioe2e/
├── Cargo.toml    # 包定义
├── build.rs      # 注入链接脚本
├── linker.ld     # 用户态段布局
└── src/
    └── main.rs   # 写入端与读取端的往返实现
```

## 相关项目

- [`audiod`](https://github.com/BRX-Boruix/audiod) —— 音频混音守护进程
- [`intel-hda`](https://github.com/BRX-Boruix/intel-hda) —— 音频硬件驱动
- [`selftest`](https://github.com/BRX-Boruix/selftest) —— 协调两端运行的宿主

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。
