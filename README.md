# audioe2e

**简体中文** | [English](#english)

BORUIX **音频子系统的端到端验收测试程序**。

内核自带的测试可以直接调用音频节点和环形缓冲，但有一条路径它们永远覆盖不到：**真实的进程
上下文切换**。音频的阻塞读取依赖进程被挂起、数据到达后再被唤醒、然后由用户态重新发起系统调用
——这只有在真正的进程里才能验证。

`audioe2e` 就是一个真实的用户态程序。它通过系统调用走完整的音频数据往返，并按角色扮演生产者
或消费者，把结果用退出码如实汇报。

---

## 它验证什么

- **系统调用包装本身** —— 参数打包、错误码翻译、用户缓冲区的合法性校验
- **真正的阻塞与唤醒往返** —— 消费者阻塞等待、数据到达、被唤醒后重新取数、校验、提交
- **跨边界的字节保真** —— 写入的内容读回来必须逐字节相同
- **背压行为** —— 输出缓冲满时生产者被正确挡住，而不是丢数据或死循环

## 使用方式

程序通过**第一个参数**选择角色：

| 参数 | 作用 |
| --- | --- |
| `producer` | 作为生产者写入一帧已知数据后退出 |
| `consumer` | 作为消费者阻塞等待数据，取回后逐字节校验，提交并退出 |
| `stream` | 持续向输出端写入确定性数据，用于落盘音频与写入内容比对 |
| `stream0` / `stream1` | 持续写入混音输入通道，验证经由混音器的链路 |
| `stream0_44k` | 以 44.1 kHz 声明并写入，验证采样率转换确实生效 |
| `stream0_sustain` / `stream1_sustain` | 长时间持续供数，用于长时间稳定性观测 |
| `stream1short` | 短暂写入后退出，用于验证"一路断开不影响另一路" |

### 退出码

| 退出码 | 含义 |
| --- | --- |
| `0` | 成功 |
| `2` | 参数无法识别 |
| 其他非零 | 如实失败 |

失败一律以非零退出码汇报，绝不把失败静默成成功，因此调用方可以直接据此断言。

## 测试数据为什么是确定性的

验收判据是"落盘的音频文件与写进去的字节**逐字节一致**"。如果写入内容含随机数或时间戳，这个
判据根本无法成立——就只能退回到"听起来有声音"，而那是一个无法自动断言的主观判断。

因此填充使用一个可复现的序列（周期 256、含位翻转），这样**字节序或偏移错位必然暴露**，
不会碰巧对上。

## 验证混音为什么不能用同一段数据

两路输入写同一序列时，相加后再乘衰减系数会**恰好还原成原序列**——这与"只混了一路"或"完全
没混"的输出一模一样，判据就失效了。

因此两路改用**幅度相差 3 倍、符号相反**的方波，相加后的结果与各种"少了一路"的情形相距很远，
四种可能的结果彼此不会混淆。

这里踩过一个坑值得记录：最初两路取 +16384 与 -16383（只差 1），二者几乎完全相消，输出近似
静音——既证明不了相加，也无法与"没有数据"区分。这个错误在开发机的单元测试里被首次运行时抓出。

## 关于采样率

写入 44.1 kHz 数据前必须**显式声明**该流的采样率。若只写数据而不声明，混音器会按默认的
48 kHz 处理，结果是音调升高约 8.8%，而链路上没有任何一处会报错——听起来只是"稍微高了一点"，
极难归因。声明与数据保持一致是生产者的责任，测试程序也不例外。

声明写的是**属性文件**而非音频数据：内核据此记录该流的真实速率，混音器读同一条属性来决定如何
重采样。两者读的是同一个事实，因此不存在"生产者和混音器对速率的理解不一致"这种可能。

## 关于生产者与消费者

音频输出端是**独占消费者**语义。这里的生产者**不需要**注册为消费者——它只需通过文件路径写入
数据即可；真正取走数据的是内核音频驱动。

这一点初期写错过：生产者调用了注册消费者的接口，而驱动已经占着那个位置，于是生产者拿到"忙"
错误、干脆一个字节都没写出去，驱动侧空转。这不是崩溃，而是**测错了对象**——测试看似在跑，
实际什么都没验证。

## 写入总量的取值

默认持续写入 8 MiB（约 43 秒音频）。早期用 512 KiB（约 2.7 秒），在验证音量调节时证明太短：
生产者很快就写完了，混音器随即进入无数据可混的空转状态，而音量变更按设计发生在混音开始一段
时间之后——那时已经没有音频，观测不到任何东西。

长时间稳定性测试另有一个 360 MiB 的模式（约 30 分钟音频）。不采用无限循环，是因为程序必须
能**自然结束**；否则只能靠外部杀进程收尾，而那样就丢失了"程序是否完整运行到尾"这个信息。

## 构建

`build.rs` 会把链接脚本传给链接器，将程序段定位到用户态地址起始处，并强制生成内核加载器
所要求的可执行文件类型。

```bash
cargo build --release
```

编译产物需要部署为 BORUIX 系统中的用户态程序，然后按上述角色参数运行。

## 文件结构

```
audioe2e/
├── Cargo.toml    # 包定义
├── build.rs      # 注入链接脚本
├── linker.ld     # 用户态段布局
└── src/
    └── main.rs   # 全部角色实现（user_main 返回进程退出码）
```

## 相关项目

- [`audiod`](../audiod) —— 用户态音频混音守护进程，本程序验证其输入链路
- [`libsys`](../libsys) —— 用户态系统调用封装

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。

---

# English

[简体中文](#audioe2e) | **English**

An **end-to-end acceptance test program for the BORUIX audio subsystem**.

The kernel's own tests can call audio nodes and ring buffers directly, but one path stays forever
out of their reach: **a real process context switch**. Blocking audio reads depend on a process
being suspended, woken when data arrives, and then re-issuing the system call from user space —
which can only be verified inside a real process.

`audioe2e` is exactly that: a real user-space program. It drives a complete audio data round trip
through system calls and, depending on the role it is given, acts as producer or consumer,
reporting the outcome faithfully through its exit code.

---

## What it verifies

- **The system call wrappers themselves** — argument packing, error code translation, and
  validation of user buffers
- **A genuine block-and-wake round trip** — the consumer blocks waiting, data arrives, it is woken,
  re-fetches, verifies, and commits
- **Byte fidelity across the boundary** — what was written must read back byte-for-byte identical
- **Backpressure behaviour** — a full output buffer correctly blocks the producer instead of
  losing data or spinning forever

## Usage

The program selects its role from its **first argument**:

| Argument | Purpose |
| --- | --- |
| `producer` | Writes one frame of known data as a producer, then exits |
| `consumer` | Blocks waiting for data as a consumer, verifies it byte-for-byte, commits, exits |
| `stream` | Continuously writes deterministic data to the output for comparison against captured audio |
| `stream0` / `stream1` | Continuously writes to a mixer input channel, exercising the path through the mixer |
| `stream0_44k` | Declares 44.1 kHz and writes at that rate, verifying sample rate conversion actually happens |
| `stream0_sustain` / `stream1_sustain` | Supplies data for an extended period, for long-run stability observation |
| `stream1short` | Writes briefly then exits, verifying that one channel disconnecting does not disturb another |

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `2` | Unrecognised argument |
| any other non-zero | An honest failure |

Failures are always reported through a non-zero exit code and never silently turned into success,
so a caller can assert on them directly.

## Why the test data is deterministic

The acceptance criterion is that the captured audio file is **byte-for-byte identical** to what was
written. If the payload contained random data or timestamps, that criterion could not hold at all —
the only fallback would be "it sounds like something", a subjective judgement no automated test can
assert.

The payload therefore uses a reproducible sequence (period 256, with bit inversions) so that **any
byte-order or offset error is guaranteed to show up** rather than coincidentally matching.

## Why verifying the mix cannot reuse the same data

When both channels write the same sequence, summing them and applying the attenuation **exactly
reproduces the original sequence** — indistinguishable from mixing only one channel, or from not
mixing at all, which defeats the criterion entirely.

The two channels therefore use square waves with a **3x amplitude difference and opposite signs**,
placing the summed result far away from every "one channel missing" case, so the possible outcomes
cannot be confused.

A pitfall worth recording: the first attempt used +16384 and -16383 (differing by just 1), which
very nearly cancelled out and produced near-silence — proving neither that summing occurred nor
that data was present at all. A host-side unit test caught this the first time it ran.

## About sample rates

Before writing 44.1 kHz data, the stream's sample rate must be **explicitly declared**. Writing the
data without declaring it makes the mixer treat it as the default 48 kHz, raising the pitch by
about 8.8% — and nothing anywhere on the path reports an error. It merely sounds slightly high,
which is very hard to trace back. Keeping the declaration consistent with the data is the
producer's responsibility, test programs included.

The declaration writes an **attribute file**, not audio data: the kernel records the stream's true
rate there, and the mixer reads that same attribute to decide how to resample. Both sides read the
same fact, so there is no possibility of the producer and the mixer disagreeing about the rate.

## About producer and consumer

The audio output has **exclusive consumer** semantics. A producer here does **not** need to register
as a consumer — it only has to write through the file path, and the kernel audio driver is what
actually takes the data out.

This was gotten wrong initially: the producer called the consumer-registration interface, the driver
already held that slot, so the producer got a "busy" error and wrote not a single byte while the
driver spun empty. That is not a crash — it is **testing the wrong thing**, where the test appears
to run while verifying nothing at all.

## How the write volume was chosen

The default continuous write is 8 MiB (about 43 seconds of audio). The early value of 512 KiB
(about 2.7 seconds) proved too short when verifying volume adjustment: the producer finished
quickly and the mixer dropped into an idle state with nothing to mix, while the volume change is by
design scheduled some time after mixing begins — by which point no audio remained and nothing
could be observed.

The long-run stability test has a separate 360 MiB mode (about 30 minutes of audio). An infinite
loop is deliberately avoided because the program must be able to **finish naturally**; otherwise the
only way to end it is killing it externally, which loses the information of whether it ran all the
way through.

## Building

`build.rs` passes the linker script to the linker, placing the program's sections at the start of
the user-space address range and forcing the executable type the kernel loader requires.

```bash
cargo build --release
```

The artifact is deployed as a user-space program in a BORUIX system and run with one of the role
arguments above.

## Layout

```
audioe2e/
├── Cargo.toml    # package definition
├── build.rs      # injects the linker script
├── linker.ld     # user-space section layout
└── src/
    └── main.rs   # all roles (user_main returns the process exit code)
```

## Related projects

- [`audiod`](../audiod) — the user-space audio mixing daemon whose input path this program verifies
- [`libsys`](../libsys) — the user-space syscall wrapper

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
