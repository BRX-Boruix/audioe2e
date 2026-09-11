//! BORUIX `audioe2e`：AUDIO 域（plan_audio_vfs.md 批次二）端到端阻塞往返测试。
//!
//! **为何需要这个程序**（S29）：内核启动期测试（`test_audio_pipe_a2`）直接调用
//! 节点与 ring，**无法**触达两条关键路径：
//!   1. AUDIO 域四个 **syscall 包装本身**（参数打包、错误码翻译、用户缓冲校验）；
//!   2. **真正的阻塞-唤醒往返**（`block_for_audio`/`wake_audio`）——它要求真实的
//!      进程上下文切换，只能在有进程上下文的用户态进程里验证。
//! 本程序正是为补这两条而存在：它经**真实 syscall** 跑完整往返。
//!
//! 用法（argv[0]）：
//!   `producer` —— 附加为消费者，经 VFS 路径写入一帧已知 PCM，退出。
//!   `consumer` —— 附加为消费者，**阻塞等待**数据，取回后逐字节校验，
//!                  commit，detach，退出（0 = 全部正确）。
//!
//! 退出码：0 = 成功；非零 = 如实失败（协调端据此断言，绝不把失败静默成成功）。

#![no_std]
#![no_main]

use libsys::{Error, OpenFlags, Permissions, STDOUT, audio, close, open, write, yield_now};

/// 测试用 PCM 帧长度（字节）。
const FRAME_BYTES: usize = 256;

/// 与写端约定的确定性填充（接收端据此逐字节校验，非随便写点数据）。
fn pattern(i: usize) -> u8 {
    // 简单可复现序列：跨 0/255 边界且有周期，字节序/偏移错误必然暴露。
    ((i * 37) ^ (i >> 3)) as u8
}

/// 两路混音验证用的**方波幅度**（s16）。
///
/// 为何不用 `pattern` 做两路混音验证：两路写同一序列时，相加再乘 1/N 会
/// **恰好还原成原序列** —— 与"只混了一路"（增益 1/1）或"直通"的输出
/// 完全一样，判据因此失效。
///
/// 改用**两个幅度相差 3 倍、符号相反**的直流方波：
///   stream/0 写 +24576 (+0.75)，stream/1 写 -8192 (-0.25)
///   相加 = +0.5，经固定增益 1/2 -> +0.25 -> s16 约 8192。
///
/// **幅度必须差别明显**：初版取 +16384 与 -16383（仅差 1），二者几乎完全
/// 相消（和仅为 1/65536），输出近似静音——那既证明不了相加，也与"没数据"
/// 无法区分。该错误由 audiod 的宿主测试在首次运行时抓出（见 lib.rs 中
/// test_m2_two_tone_acceptance_matches_end_to_end_scenario 的说明）。
///
/// 判据的四种可能结果彼此远离，无歧义：
///   两路都在   -> +0.25 -> 约 8192
///   仅 stream/0 -> +0.75 -> 约 24575
///   仅 stream/1 -> -0.25 -> 约 -8192
///   都没有     -> 完全没有字节写出
const MIX_TEST_TONE_HI: i16 = 24576;
const MIX_TEST_TONE_LO: i16 = -8192;

/// 按本路角色填充一帧立体声方波（s16 小端交错）。
///
/// `hi` 为 true 表示"高幅正相"路（stream/0），false 表示"低幅反相"路
/// （stream/1）。两路都填**左右相同**的值：单声道内容复制到双声道，
/// 与 audiod 混音核心的 mono->stereo 约定一致。
fn fill_tone(buf: &mut [u8], hi: bool, frames: usize) {
    let v: i16 = if hi { MIX_TEST_TONE_HI } else { MIX_TEST_TONE_LO };
    let le = v.to_le_bytes();
    let mut f = 0usize;
    while f < frames {
        let o = f * 4;
        buf[o] = le[0];
        buf[o + 1] = le[1];
        buf[o + 2] = le[0];
        buf[o + 3] = le[1];
        f += 1;
    }
}

/// 输出一行到 stdout。
fn say(s: &[u8]) {
    let _ = write(STDOUT, s);
    let _ = write(STDOUT, b"\n");
}

/// 输出一个十进制数（不换行）。用于进度汇报——让"推进"可见而不是一个静止的数字。
fn say_dec(mut n: usize) {
    if n == 0 {
        let _ = write(STDOUT, b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _ = write(STDOUT, &buf[i..]);
}

/// 打开标志：读写（dsp 节点同时支持读写）。
const O_RDWR: OpenFlags = OpenFlags::READ_WRITE;

/// 用户程序入口（libsys `_start` 调用）。返回值为进程退出码。
#[unsafe(no_mangle)]
pub extern "C" fn user_main(argc: isize, argv: *const *const u8) -> i32 {
    let cmd: &[u8] = unsafe {
        if argc < 1 || argv.is_null() {
            &[]
        } else {
            let p = *argv;
            if p.is_null() {
                &[]
            } else {
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                core::slice::from_raw_parts(p, len)
            }
        }
    };

    if cmd == b"producer" {
        run_producer()
    } else if cmd == b"consumer" {
        run_consumer()
    } else if cmd == b"stream" {
        // A3 直连：写 dsp（保持既有验收基线不变）。
        run_stream(STREAM_TOTAL_BYTES, "/devices/audio/dsp", None, None)
    } else if cmd == b"stream0" {
        // M1 混音链路：写 stream/0，由 audiod 转发到 dsp。
        //
        // **为何是单 token 而非 `stream <n> <target>`**：内核的参数块 ABI
        // （docs/abi/syscall-abi.md §4）**恒设 argc=1**，整条 cmd 字符串作为
        // argv[0] 原样交付，不按空格切分（loader/src/lib.rs:844-846）。
        // 因此多词命令行会整条进入 `cmd`，与 `b"stream"` 精确比较必然不匹配
        // ——实测正是 `FAIL: unknown mode`。
        //
        // 本批次不改 ABI：那是已文档化的契约，改它需同步文档与全部用户程序，
        // 属独立变更（S24 单组件专注）。故用**单 token 表达"模式+目标"**，
        // 各模式用各自确定的字节数常量（STREAM_TOTAL_BYTES），不引入额外参数。
        run_stream(STREAM_TOTAL_BYTES, "/devices/audio/stream/0", Some(true), None)
    } else if cmd == b"stream1" {
        // M2 第二路：写 stream/1，与 stream/0 在 audiod 内相加。
        // 两路的 pattern 相位相同，但 M2 混音测试程序会写**可区分的**内容，
        // 使"两路真的都进来了"可从输出反推（见 audioe2e stream2 模式）。
        run_stream(STREAM_TOTAL_BYTES, "/devices/audio/stream/1", Some(false), None)
    } else if cmd == b"stream0_44k" {
        // 批次五 M5：以 **44.1kHz** 声明并写入 stream/0，验证 audiod 会按
        // rate 属性把它重采样到 48kHz。
        //
        // 为何用它作端到端验证：若重采样没生效（或 audiod 误以为源是 48k），
        // 输出会**变快约 8.8%** 且时间轴缩短；若生效，输出时长与 48k 源一致。
        // 这是可以从落盘 WAV 的**长度**直接判定的差异，不依赖听感。
        run_stream(STREAM_TOTAL_BYTES, "/devices/audio/stream/1", Some(false), Some(44_100))
    } else if cmd == b"stream1short" {
        // M2 出口条件「一路断开不影响另一路」的验证用：
        // stream/1 只写一小段就退出，stream/0 仍持续写。
        //
        // 期望观察：stream/1 退出后，audiod 的 live_inputs 从 2 变 1，
        // 混音输出从 +0.25(8191) 变为 +0.75(24575) —— 增益随**实际参与
        // 的路数**变化，而 stream/0 的内容本身不受任何影响。
        run_stream(SHORT_STREAM_BYTES, "/devices/audio/stream/1", Some(false), None)
    } else {
        say(b"[audioe2e] FAIL: unknown mode (want producer|consumer|stream|stream0|stream0_44k|stream1|stream1short)");
        2
    }
}

/// 写端：附加 → 经 VFS 路径写入已知 PCM → 退出（槽位由 S18 自动回收）。
fn run_producer() -> i32 {
    if audio::attach().is_err() {
        say(b"[audioe2e] FAIL: attach (producer) rejected");
        return 1;
    }
    say(b"[audioe2e] producer attached");

    // 经 **VFS 路径**打开 dsp 并写入：验证 VFS 数据通路与 AUDIO 控制通路
    // 确实协同工作（两条通路分开实现，只有合起来才构成真正的音频管道）。
    let path = b"/devices/audio/dsp\0";
    let fd = match open(
        unsafe { core::str::from_utf8_unchecked(&path[..path.len() - 1]) },
        O_RDWR,
        Permissions::read_write(),
    ) {
        Ok(f) => f,
        Err(_) => {
            say(b"[audioe2e] FAIL: open /devices/audio/dsp");
            let _ = audio::detach();
            return 1;
        }
    };

    let mut frame = [0u8; FRAME_BYTES];
    for (i, b) in frame.iter_mut().enumerate() {
        *b = pattern(i);
    }
    let n = match write(fd, &frame) {
        Ok(n) => n,
        Err(_) => {
            say(b"[audioe2e] FAIL: write to dsp rejected");
            let _ = audio::detach();
            return 1;
        }
    };
    if n != FRAME_BYTES {
        say(b"[audioe2e] FAIL: short write (expected full frame)");
        let _ = audio::detach();
        return 1;
    }
    say(b"[audioe2e] producer wrote full frame via VFS path");

    // 不 detach：consumer 需要它仍在附加状态才能取数。
    // 由进程退出路径（S18）自动回收——这本身也顺带验证了退出清理。
    say(b"[audioe2e] producer exiting (slot auto-released by S18 cleanup)");
    0
}

/// 读端：附加 → **阻塞**取数 → 逐字节校验 → commit → detach。
fn run_consumer() -> i32 {
    if audio::attach().is_err() {
        say(b"[audioe2e] FAIL: attach (consumer) rejected");
        return 1;
    }
    say(b"[audioe2e] consumer attached");

    let mut buf = [0u8; FRAME_BYTES];
    // 阻塞取数 + **重试**。
    //
    // 这是本 API 契约的核心，也是初版 e2e 写错的地方（值得留档）：内核把本进程
    // 置 Blocked 切走后，数据到达时 `wake_audio` 唤醒它，但**唤醒不等于交付**
    // ——`saved.rax` 被预置为 `-EAGAIN`（"曾阻塞、请重试"），syscall 如实返回
    // EAGAIN，由用户态**重新发起 fetch** 取数据。
    //
    // 为何这样设计（而非让内核直接带着数据返回）：唤醒发生在中断/写者上下文，
    // 彼时无法安全地把数据写进**此进程**的用户缓冲（地址空间可能不在当前 CR3）。
    // 故内核只负责"叫醒"，交付由被唤醒者在自己的上下文里完成——这也是
    // `block_for_event` 既有约定的同构做法。
    //
    // 因此：EAGAIN 必须重试；其它错误才是真失败。
    // 初值 0 是**死赋值**（编译期警告指出）：循环里每条成功路径都会先赋 `n`
    // 再 break，故 0 永远读不到。改用 loop 的 break 值直接给出结果，
    // 省掉一个可变绑定——少一个可变状态就少一类错。
    let mut attempts = 0u32;
    let n = loop {
        match audio::fetch(&mut buf) {
            Ok(k) => {
                break k;
            }
            Err(e) if e == Error::WouldBlock => {
                // EAGAIN："曾阻塞、请重试"或超时。二者都重试即可——
                // 有数据时下一次 fetch 立即成功。
                attempts += 1;
                if attempts > 64 {
                    say(b"[audioe2e] FAIL: fetch EAGAIN retry limit exceeded");
                    let _ = audio::detach();
                    return 1;
                }
                continue;
            }
            Err(_) => {
                say(b"[audioe2e] FAIL: fetch returned a hard error (not EAGAIN)");
                let _ = audio::detach();
                return 1;
            }
        }
    };
    if n != FRAME_BYTES {
        say(b"[audioe2e] FAIL: fetch returned wrong length");
        let _ = audio::detach();
        return 1;
    }
    say(b"[audioe2e] consumer fetched full frame");

    // 逐字节校验：任何不一致都如实失败，绝不"大致对就算过"。
    for (i, b) in buf.iter().enumerate() {
        if *b != pattern(i) {
            say(b"[audioe2e] FAIL: PCM payload mismatch (byte-for-byte)");
            let _ = audio::detach();
            return 1;
        }
    }
    say(b"[audioe2e] consumer verified payload byte-for-byte");

    // 两阶段：确认无误后才 commit。
    if audio::commit(n).is_err() {
        say(b"[audioe2e] FAIL: commit rejected");
        let _ = audio::detach();
        return 1;
    }
    say(b"[audioe2e] consumer committed");

    if audio::detach().is_err() {
        say(b"[audioe2e] FAIL: detach rejected");
        return 1;
    }
    say(b"[audioe2e] PASS: attach/fetch/verify/commit/detach round-trip OK");
    0
}

// `parse_dec` 已删除（批次四 M1）：它唯一的用途是解析此前设想的 `argv[1]`
// 字节数参数，而内核参数块 ABI 恒设 argc=1、不做空格切分（见 user_main 中
// `stream0` 分支的说明）。参数取消后它成为**死代码**，编译警告如实指出。
// 保留一个未被调用的函数违反 S06/S39；如将来需要通用数字参数，
// 应作为带文档的 ABI 变更重新引入，而非留在这里当"将来可能用"。
/// 流式写端：持续向 dsp 写入**已知的确定性 PCM**，供 A3 的 WAV 比对验收。
///
/// **为何必须确定性**：A3 的验收判据是"QEMU 落盘的 WAV 与写进去的字节
/// **逐字节一致**"。若填充含随机数或时间戳，这个判据就无法成立——只能退回
/// 到"听起来有声音"，那是不可自动断言的主观判断。
///
/// 填充用 `pattern(i)`（周期 256、含位翻转）：字节序或偏移错位必然暴露，
/// 不会"碰巧对上"。
///
/// 模式 token：`stream`（写 dsp，A3 直连）/ `stream0`（写 stream/0，M1 混音链路）。

/// 声明某一路流的采样率（写它的 `rate` 属性）。
///
/// 写的是**属性文件**，不是音频数据：内核据此记录该流的真实速率，
/// audiod 读同一条属性来决定如何重采样。两者读的是同一个事实，
/// 故不存在「生产者和混音器对速率的理解不一致」这种可能。
///
/// 失败如实返回错误，不静默继续：一个速率声明失败却照样写 PCM 的生产者，
/// 会让整条链路按错误速度播放且无人察觉（S20）。
///
/// `stream_path` 是流本身的路径（如 `/devices/audio/stream/0`），
/// 本函数在其后拼 `/rate`。用定长缓冲拼接，不引入分配器。
fn set_stream_rate(stream_path: &str, rate_hz: u32) -> Result<(), ()> {
    const PATH_MAX: usize = 64;
    if stream_path.len() + 6 > PATH_MAX {
        return Err(());
    }
    let mut pbuf = [0u8; PATH_MAX];
    let n = stream_path.len();
    pbuf[..n].copy_from_slice(stream_path.as_bytes());
    pbuf[n..n + 5].copy_from_slice(b"/rate");
    let path = unsafe { core::str::from_utf8_unchecked(&pbuf[..n + 5]) };
    // 只写：本函数只声明速率，不读回。
    let fd = open(path, OpenFlags::WRITE_ONLY, Permissions::read_write()).map_err(|_| ())?;
    let mut rbuf = [0u8; 8];
    let len = format_u32(rate_hz, &mut rbuf);
    let res = write(fd, &rbuf[..len]).map_err(|_| ());
    let _ = close(fd);
    res.map(|_| ())
}

/// 把 `v` 以十进制写入 `buf`，返回写入长度。
///
/// 不引入 `alloc`（本 crate 未链接分配器），故手写最小十进制转换。
/// 只处理无符号十进制，这正是 `rate` 属性需要的全部。
fn format_u32(v: u32, buf: &mut [u8]) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    // 先逆序生成，再翻转。u32 最多 10 位。
    let mut tmp = [0u8; 10];
    let mut n = 0usize;
    let mut x = v;
    while x > 0 {
        tmp[n] = b'0' + (x % 10) as u8;
        x /= 10;
        n += 1;
    }
    let mut i = 0usize;
    while i < n {
        buf[i] = tmp[n - 1 - i];
        i += 1;
    }
    n
}
/// 写够即退出——退出即触发 S18 回收槽位，驱动侧会观察到"写者消失"。
fn run_stream(
    limit_bytes: usize,
    target_path: &str,
    tone: Option<bool>,
    rate_hz: Option<u32>,
) -> i32 {
    // **生产者不 attach**——这是 A2 的架构语义，初版写错在此留档。
    //
    // attach() 是把自己注册为 ring 的**独占消费者**。而本程序是**生产者**：
    // 消费者是 intel-hda 驱动（它从 ring 取数喂 DMA）。生产者只需经 VFS 路径
    // `write` 写入，而写路径只要求"**存在**消费者"（`is_attached()`），
    // **不要求写者就是消费者**。
    //
    // 初版这里调了 attach()，结果是：驱动已占着消费者槽 → 本进程拿到 EBUSY →
    // 干脆没写任何数据 → 驱动侧空转欠载。这不是崩溃，而是**测错了对象**。
    say(b"[audioe2e] stream producer starting (no attach: we are the producer, not the consumer)");

    // **写入目标可指定**（批次四 M1）：
    //   - 默认 `dsp`  —— A3 的直连路径（生产者直接喂驱动）；
    //   - `stream0`   —— M1 的混音链路（生产者 -> stream/0 -> audiod -> dsp）。
    //
    // 为何要参数化而不是改默认值：A3 的验收证据建立在"直连 dsp"之上，
    // 直接改掉会让 A3 回归失去对照。两条路径都要保留、都要可复现。
    // 选择经**显式参数**而非环境探测（S16：自动逻辑必须提供手动覆盖，
    // 且默认行为要有理由——此处默认保持 dsp 正是为了不破坏 A3 基线）。
    let target: &str = target_path;
    // 不用 alloc：本 crate 未链接分配器。目标路径是编译期已知的两个字面量之一，
    // 直接在定长数组里拼 NUL 结尾即可（open 需要 NUL 终止的字节串）。
    const TARGET_MAX: usize = 32;
    if target.len() + 1 > TARGET_MAX {
        say(b"[audioe2e] FAIL: target path too long (internal invariant)");
        return 1;
    }
    let mut pbuf = [0u8; TARGET_MAX];
    pbuf[..target.len()].copy_from_slice(target.as_bytes());
    let fd = match open(
        unsafe { core::str::from_utf8_unchecked(&pbuf[..target.len()]) },
        O_RDWR,
        Permissions::read_write(),
    ) {
        Ok(f) => f,
        Err(_) => {
            say(b"[audioe2e] FAIL: open stream target");
            return 1;
        }
    };
    say(b"[audioe2e] producer target opened");

    // 批次五 M5：可选地先把该流的采样率**声明**出来，再写数据。
    //
    // 为何必须显式声明而不是"写了就算"：audiod 按 rate 属性重采样。
    // 若生产者写 44.1k 的样本却不声明，audiod 会按默认 48000 处理，
    // 结果是**音调升高约 8.8%**（44100/48000 的倒数），而链路上没有任何
    // 一处会报错。声明与数据一致是生产者的责任，测试也一样。
    if let Some(rate) = rate_hz {
        if set_stream_rate(target, rate).is_err() {
            say(b"[audioe2e] FAIL: could not declare stream sample rate");
            return 1;
        }
    }

    // ring 只有 64KiB；写满即 WouldBlock（背压）。故按块写并重试，
    // 这是**真实的流式行为**：生产者必须能被背压挡住。
    const CHUNK: usize = 4096;
    let mut buf = [0u8; CHUNK];
    let mut written = 0usize;
    let mut blocked_rounds = 0u32;
    while written < limit_bytes {
        let want = core::cmp::min(CHUNK, limit_bytes - written);
        match tone {
            // 混音验证：写两路幅度不同、符号相反的方波（见 MIX_TEST_TONE_*）。
            // 必须**帧对齐**（want 是 4 的倍数），否则会写出半帧。
            Some(hi) => {
                debug_assert!(want % 4 == 0, "tone chunk must be frame-aligned");
                fill_tone(&mut buf[..want], hi, want / 4);
            }
            // A3 直连：写确定性 pattern，供驱动侧逐字节校验。
            None => {
                for (i, b) in buf[..want].iter_mut().enumerate() {
                    *b = pattern(written + i);
                }
            }
        }
        match write(fd, &buf[..want]) {
            Ok(0) => {
                // 短写 0：ring 满。让出后重试（背压）。
                blocked_rounds += 1;
                if blocked_rounds > 200_000 {
                    say(b"[audioe2e] FAIL: stream stalled (ring full for too long)");
                    return 1;
                }
                let _ = yield_now();
            }
            Ok(n) => {
                written += n;
                blocked_rounds = 0;
            }
            Err(e) if e == Error::WouldBlock => {
                // EAGAIN：ring 满。**这是正常的背压**，让出重试。
                blocked_rounds += 1;
                if blocked_rounds > 200_000 {
                    say(b"[audioe2e] FAIL: stream stalled (EAGAIN for too long)");
                    return 1;
                }
                let _ = yield_now();
            }
            Err(_) => {
                say(b"[audioe2e] FAIL: write to dsp returned a hard error");
                return 1;
            }
        }
        // 周期性汇报进度（证明真的在推进，而非停在某个数字上）。
        if written % (64 * 1024) < CHUNK {
            say(b"[audioe2e] stream wrote ");
            say_dec(written);
            say(b" bytes");
        }
    }
    say(b"[audioe2e] stream wrote ALL ");
    say_dec(written);
    say(b" bytes; exiting (S18 will release the slot)");
    0
}

/// 默认流式写入总量：8 MiB。
///
/// 早期为 512 KiB（≈2.7 秒）。在 M4 的音量斜坡观测中证明**太短**：
/// audiod 只搬了 593920 字节（约 3 秒）生产者就写完了，此后混音器进入
/// live=0 空转，而音量变更按设计发生在"已混音 64 轮"之后 —— 那时已无
/// 音频，端到端观测什么也证明不了。
///
/// 8 MiB ≈ 43 秒音频，足以让音量变更前后各有漫长的稳态供数窗口，
/// 并且远超 intel-hda 每 4 圈（约 2.7 秒）一次的稳态采样间隔。
/// 代价是每次运行多花几十秒，相对于它带来的可观测性是值得的。
const STREAM_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// In the early-disconnect test, how many bytes the short-lived path writes.
///
/// Sized to span SEVERAL of audiod progress-report intervals (every 64 rounds,
/// each round moving up to 4 KiB per path). 64 KiB drained before the first
/// report appeared, so the log showed only live_inputs=1 and the TRANSITION
/// from 2 to 1 -- the thing the test exists to observe -- was never captured.
/// 2 MiB keeps the short path alive for many reports, so the log contains both
/// live_inputs=2 (while it runs) and live_inputs=1 (after it exits), which is
/// what actually demonstrates "one path disconnecting does not disturb the other".
const SHORT_STREAM_BYTES: usize = 2 * 1024 * 1024;