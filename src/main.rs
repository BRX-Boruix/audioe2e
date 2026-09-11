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

use libsys::{Error, OpenFlags, Permissions, STDOUT, audio, open, write, yield_now};

/// 测试用 PCM 帧长度（字节）。
const FRAME_BYTES: usize = 256;

/// 与写端约定的确定性填充（接收端据此逐字节校验，非随便写点数据）。
fn pattern(i: usize) -> u8 {
    // 简单可复现序列：跨 0/255 边界且有周期，字节序/偏移错误必然暴露。
    ((i * 37) ^ (i >> 3)) as u8
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
        // 可选第二参数：总字节数（十进制）。默认 STREAM_TOTAL_BYTES。
        let limit = if argc >= 2 {
            let a = unsafe {
                let p = *argv.add(1);
                if p.is_null() {
                    None
                } else {
                    let mut len = 0usize;
                    while *p.add(len) != 0 {
                        len += 1;
                    }
                    Some(core::slice::from_raw_parts(p, len))
                }
            };
            match a {
                Some(s) => parse_dec(s).unwrap_or(STREAM_TOTAL_BYTES),
                None => STREAM_TOTAL_BYTES,
            }
        } else {
            STREAM_TOTAL_BYTES
        };
        run_stream(limit)
    } else {
        say(b"[audioe2e] FAIL: unknown mode (want producer|consumer|stream)");
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
    let mut n = 0usize;
    let mut attempts = 0u32;
    loop {
        match audio::fetch(&mut buf) {
            Ok(k) => {
                n = k;
                break;
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
    }
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

/// 解析十进制字符串；含非数字则返回 None（不猜、不容错到"看起来对"）。
fn parse_dec(s: &[u8]) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((c - b'0') as usize)?;
    }
    Some(n)
}


/// 流式写端：持续向 dsp 写入**已知的确定性 PCM**，供 A3 的 WAV 比对验收。
///
/// **为何必须确定性**：A3 的验收判据是"QEMU 落盘的 WAV 与写进去的字节
/// **逐字节一致**"。若填充含随机数或时间戳，这个判据就无法成立——只能退回
/// 到"听起来有声音"，那是不可自动断言的主观判断。
///
/// 填充用 `pattern(i)`（周期 256、含位翻转）：字节序或偏移错位必然暴露，
/// 不会"碰巧对上"。
///
/// `argv[1]` 可给总字节数（十进制）。默认写 `STREAM_TOTAL_BYTES`。
/// 写够即退出——退出即触发 S18 回收槽位，驱动侧会观察到"写者消失"。
fn run_stream(limit_bytes: usize) -> i32 {
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

    let path = b"/devices/audio/dsp\0";
    let fd = match open(
        unsafe { core::str::from_utf8_unchecked(&path[..path.len() - 1]) },
        O_RDWR,
        Permissions::read_write(),
    ) {
        Ok(f) => f,
        Err(_) => {
            say(b"[audioe2e] FAIL: open /devices/audio/dsp");
            return 1;
        }
    };

    // ring 只有 64KiB；写满即 WouldBlock（背压）。故按块写并重试，
    // 这是**真实的流式行为**：生产者必须能被背压挡住。
    const CHUNK: usize = 4096;
    let mut buf = [0u8; CHUNK];
    let mut written = 0usize;
    let mut blocked_rounds = 0u32;
    while written < limit_bytes {
        let want = core::cmp::min(CHUNK, limit_bytes - written);
        for (i, b) in buf[..want].iter_mut().enumerate() {
            *b = pattern(written + i);
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

/// 默认流式写入总量：512 KiB。
///
/// 为何选它：48kHz/16bit/立体声 = 192000 B/s，512KiB ≈ 2.7 秒音频。
/// 足够长到能观察到多轮双缓冲轮转（每块 16KiB，共 32 轮），又不至于让
/// 测试跑太久。
const STREAM_TOTAL_BYTES: usize = 512 * 1024;