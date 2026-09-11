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

use libsys::{Error, OpenFlags, Permissions, STDOUT, audio, open, write};

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
    } else {
        say(b"[audioe2e] FAIL: unknown mode (want producer|consumer)");
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
