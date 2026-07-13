import { useEffect, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import {
  Archive, ArrowDownToLine, Check, ChevronDown, CircleAlert, CircleCheck,
  Database, FileClock, FolderOpen, LockKeyhole, Play, RotateCcw,
  ShieldCheck, Sparkles, Terminal, X
} from 'lucide-react'

type Provider = { id: string; name: string; color: string; sessions: number; indexed: number; status: string }
type LockDetail = { state: string; path: string; ownerPid?: number; ageSeconds?: number; activeProcesses: string[] }
type Source = { name: string; path: string; records: number; readable: boolean; note: string }
type Scan = {
  codexHome: string; currentProvider: string; providers: Provider[]; sessions: number;
  discoveredSessions: number; orphanedSessions: number;
  archivedSessions: number; ordinarySessions: number; recoverableSessions: number; recoverableIndexed: number;
  sessionIndexCovered: number; remoteSessions: number; remoteExcludedSessions: number; automatedSessions: number;
  rolloutSessions: number; validRolloutSessions: number; indexed: number; sessionIndexed: number;
  drift: number; providerDrift: number; rolloutProviderDrift: number; missingCatalog: number; skipped: number;
  missingRollout: number;
  sqlite: number; jsonl: number; lock: string; lockDetail: LockDetail;
  needsAdmin: boolean; lastBackup?: string; sources: Source[]
}
type RepairResult = {
  changed: number; providersFixed: number; indexAdded: number; skipped: number;
  skippedReasons: { threadId?: string; reason: string }[]; dryRun: boolean;
  verified: boolean; backupPath?: string; lock: string; needsAdmin: boolean
}
type VerifyResult = { ok: boolean; checked: number; remaining: number; skipped: number; reasons: { threadId?: string; reason: string }[] }
type LogEntry = { time: string; tone: 'ok' | 'warn' | 'info'; text: string }

const emptyScan: Scan = {
  codexHome: '未发现', currentProvider: 'unknown', sessions: 0, discoveredSessions: 0, orphanedSessions: 0, archivedSessions: 0,
  ordinarySessions: 0, recoverableSessions: 0, recoverableIndexed: 0, sessionIndexCovered: 0,
  remoteSessions: 0, remoteExcludedSessions: 0, automatedSessions: 0,
  rolloutSessions: 0, validRolloutSessions: 0, indexed: 0, sessionIndexed: 0, drift: 0, providerDrift: 0,
  missingCatalog: 0, missingRollout: 0, rolloutProviderDrift: 0, skipped: 0, sqlite: 0, jsonl: 0, lock: 'clear',
  lockDetail: { state: 'clear', path: '', activeProcesses: [] }, needsAdmin: false,
  sources: [],
  providers: [
    { id: 'custom', name: 'Custom', color: '#2d7b6f', sessions: 0, indexed: 0, status: 'available' },
    { id: 'openai', name: 'OpenAI', color: '#4779a7', sessions: 0, indexed: 0, status: 'available' },
    { id: 'codexpilot', name: 'CodexPilot', color: '#b17842', sessions: 0, indexed: 0, status: 'available' }
  ]
}

function formatTime() { return new Date().toLocaleTimeString('zh-CN', { hour12: false }) }
function isTauriDesktop() {
  return Boolean((window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__)
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauriDesktop()) throw new Error('请从 Tauri 桌面端启动此工具')
  return invoke<T>(command, args)
}

export default function App() {
  const [scan, setScan] = useState<Scan>(emptyScan)
  const [selected, setSelected] = useState('openai')
  const [logs, setLogs] = useState<LogEntry[]>([{ time: formatTime(), tone: 'info', text: '等待桌面端扫描 CODEX_HOME' }])
  const [busy, setBusy] = useState<'scan' | 'backup' | 'repair' | 'rollback' | 'verify' | null>('scan')
  const [dryRun, setDryRun] = useState(true)
  const [showAllLogs, setShowAllLogs] = useState(false)
  const [toast, setToast] = useState<string | null>(null)

  const selectedProvider = scan.providers.find(provider => provider.id === selected) ?? scan.providers[0]
  const addLog = (tone: LogEntry['tone'], text: string) => setLogs(prev => [{ time: formatTime(), tone, text }, ...prev])
  const logLockState = (value: Scan) => {
    if (value.lockDetail.activeProcesses.includes('process-enumeration-failed')) {
      addLog('warn', '无法枚举 Codex 进程，写入已锁定；可能需要管理员权限')
    } else if (value.lockDetail.activeProcesses.length > 0) {
      const names = value.lockDetail.activeProcesses.slice(0, 4).join('、')
      const extra = value.lockDetail.activeProcesses.length > 4 ? ` 等 ${value.lockDetail.activeProcesses.length} 个进程` : ''
      addLog('warn', `写入已锁定：请先关闭 ${names}${extra}`)
    } else if (value.needsAdmin) {
      addLog('warn', '部分来源无权访问，可能需要管理员权限')
    }
  }

  const applyScan = (value: Scan) => {
    setScan(value)
    setSelected(value.providers.some(provider => provider.id === value.currentProvider) ? value.currentProvider : 'openai')
  }

  useEffect(() => {
    let mounted = true
    call<Scan>('scan_codex')
      .then(value => { if (mounted) { applyScan(value); addLog('ok', `扫描完成：${value.recoverableSessions} 条可恢复，${value.missingCatalog} 条待恢复，${value.remoteSessions} 条远端映射`); logLockState(value) } })
      .catch(error => { if (mounted) { addLog('warn', String(error)); setToast(String(error)) } })
      .finally(() => { if (mounted) setBusy(null) })
    return () => { mounted = false }
  }, [])

  const runScan = async () => {
    setBusy('scan')
    try {
      const result = await call<Scan>('scan_codex')
      applyScan(result)
      addLog('ok', `扫描完成：${result.recoverableSessions} 条可恢复，侧栏覆盖 ${result.recoverableIndexed}/${result.recoverableSessions}，远端 ${result.remoteSessions} 条`)
      logLockState(result)
      setToast('扫描完成')
    } catch (error) {
      addLog('warn', `扫描失败：${String(error)}`); setToast('扫描失败')
    } finally { setBusy(null) }
  }

  const createBackup = async () => {
    setBusy('backup')
    try {
      const result = await call<{ path: string }>('create_backup')
      setScan(prev => ({ ...prev, lastBackup: result.path }))
      addLog('ok', `SQLite 在线快照已创建 · ${result.path}`)
      setToast('备份完成')
    } catch (error) {
      addLog('warn', `备份失败：${String(error)}`); setToast('备份失败')
    } finally { setBusy(null) }
  }

  const verify = async (provider = selected) => {
    setBusy('verify')
    try {
      const result = await call<VerifyResult>('verify_codex', { targetProvider: provider })
      addLog(result.ok ? 'ok' : 'warn', result.ok ? `验证通过：检查 ${result.checked} 条候选` : `验证未通过：仍有 ${result.remaining} 条记录待处理`)
      setToast(result.ok ? '验证通过' : '仍有记录待处理')
    } catch (error) {
      addLog('warn', `验证失败：${String(error)}`); setToast('验证失败')
    } finally { setBusy(null) }
  }

  const repair = async () => {
    if (!dryRun && !window.confirm('将创建 SQLite 快照并修改两套本地索引数据库。请先关闭 Codex 相关进程；失败时会尝试自动回滚。继续吗？')) return
    setBusy('repair')
    try {
      const result = await call<RepairResult>('repair_indexes', { targetProvider: selected, dryRun })
      const mode = dryRun ? '预览' : '同步'
      addLog(dryRun ? 'info' : 'ok', `${mode}完成：${result.changed} 条变更，provider ${result.providersFixed} 条，新增索引 ${result.indexAdded} 条，跳过 ${result.skipped} 条`)
      if (result.backupPath) setScan(prev => ({ ...prev, lastBackup: result.backupPath }))
      if (!dryRun) {
        const refreshed = await call<Scan>('scan_codex')
        applyScan(refreshed)
        addLog(result.verified ? 'ok' : 'warn', result.verified ? '写入后验证通过' : '写入完成但验证未通过')
      }
      setToast(dryRun ? '预览完成' : result.verified ? '同步并验证完成' : '同步后仍需检查')
    } catch (error) {
      const message = String(error)
      const restored = /restored backup|was restored|已恢复|已回滚/i.test(message) && !/restore failed|回滚失败/i.test(message)
      addLog('warn', `修复失败：${message}`); setToast(restored ? '修复失败，已自动回滚' : '修复失败，详情见日志')
    } finally { setBusy(null) }
  }

  const rollback = async () => {
    setBusy('rollback')
    try {
      const result = await call<VerifyResult>('rollback_latest')
      const refreshed = await call<Scan>('scan_codex')
      applyScan(refreshed)
      addLog(result.ok ? 'ok' : 'warn', result.ok ? '已回滚并通过当前 provider 验证' : `已回滚；当前 provider 仍有 ${result.remaining} 条未对齐`)
      setToast('已回滚')
    } catch (error) {
      addLog('warn', `回滚失败：${String(error)}`); setToast('回滚失败')
    } finally { setBusy(null) }
  }

  const visibleLogs = showAllLogs ? logs : logs.slice(0, 3)
  if (!isTauriDesktop()) {
    return <div className="desktop-only"><div className="desktop-only-mark"><Sparkles size={20} /></div><h1>Codex Provider Hub</h1><p>请从已安装的 Tauri 桌面端启动，不要直接打开开发服务器地址。</p></div>
  }

  const currentProvider = scan.providers.find(provider => provider.id === scan.currentProvider)
  const globalState = scan.sources.find(source => source.name === 'global_state')
  const sessionIndex = scan.sources.find(source => source.name === 'session_index')
  const writeBlocked = scan.lock !== 'clear' || scan.needsAdmin || scan.sqlite < 2 || scan.codexHome === '未发现'
  const catalogCoverage = scan.recoverableSessions === 0
    ? '-'
    : `${Math.round((scan.recoverableIndexed / scan.recoverableSessions) * 100)}%`
  return <div className="app-shell">
    <header className="topbar">
      <div className="brand"><div className="brand-mark"><Sparkles size={17} /></div><div><strong>Codex Provider Hub</strong><span>session repair utility <em>v0.1</em></span></div></div>
      <div className="top-actions"><span className="runtime"><span className="pulse" />本地运行中</span><span className="avatar" title="本地桌面端">XC</span></div>
    </header>

    <main className="workspace">
      <section className="intro-row"><div><p className="eyebrow">SESSION VISIBILITY / SAFE RECOVERY</p><h1>让历史会话重新可见。</h1><p className="subhead">对齐 provider 索引，保留原始 JSONL，不改动凭据与配置。</p></div><div className="intro-actions"><button className="secondary-button" onClick={runScan} disabled={busy !== null}><RotateCcw size={16} className={busy === 'scan' ? 'spin' : ''} />{busy === 'scan' ? '扫描中' : '重新扫描'}</button><button className="secondary-button" onClick={() => verify()} disabled={busy !== null}><ShieldCheck size={16} />{busy === 'verify' ? '验证中' : '验证'}</button></div></section>

      <section className="path-strip"><div className="path-icon"><FolderOpen size={16} /></div><div><span>CODEX_HOME</span><strong>{scan.codexHome}</strong></div><div className="strip-divider" /><div><span>当前 Provider</span><strong className="provider-inline"><i style={{ background: currentProvider?.color }} />{currentProvider?.name ?? scan.currentProvider}</strong></div><div className="strip-status"><ShieldCheck size={15} />只读扫描</div></section>

      <section className="metrics" aria-label="扫描概览"><div className="metric"><span>可恢复会话</span><strong>{scan.recoverableSessions}</strong><small>state {scan.sessions} · 普通候选 {scan.ordinarySessions}</small></div><div className="metric"><span>侧栏覆盖</span><strong>{scan.recoverableIndexed}/{scan.recoverableSessions}</strong><small>{catalogCoverage} · session index {scan.sessionIndexCovered}</small></div><div className="metric warn"><span>待恢复</span><strong>{scan.missingCatalog}</strong><small>provider {scan.providerDrift} · 内容异常 {scan.missingRollout}</small></div><div className="metric"><span>远端映射</span><strong>{scan.remoteSessions}</strong><small>排除 {scan.remoteExcludedSessions} · 归档 {scan.archivedSessions} · 孤儿 {scan.orphanedSessions}</small></div></section>

      <div className="content-grid">
        <section className="main-column">
          <div className="section-heading"><div><p className="eyebrow">01 / TARGET</p><h2>选择目标 Provider</h2></div><span className="helper">仅处理 allowlist 内的普通本地线程</span></div>
          <div className="provider-list">{scan.providers.map(provider => <button key={provider.id} className={`provider-row ${selected === provider.id ? 'selected' : ''}`} onClick={() => setSelected(provider.id)} disabled={busy !== null}><span className="provider-color" style={{ background: provider.color }} /><span className="provider-copy"><strong>{provider.name}</strong><small>{provider.id} · {provider.status === 'active' ? '当前配置' : provider.status === 'legacy' ? '历史来源' : '可用来源'}</small></span><span className="provider-count"><strong>{provider.sessions}</strong><small>可恢复</small></span><span className="provider-index"><strong className={provider.sessions !== provider.indexed ? 'text-warn' : ''}>{provider.indexed}</strong><small>已索引</small></span><span className="radio">{selected === provider.id && <Check size={13} />}</span></button>)}</div>

          <div className="section-heading action-heading"><div><p className="eyebrow">02 / RECOVERY</p><h2>安全同步</h2></div><div className="dry-toggle"><span>预览模式</span><button className={`toggle ${dryRun ? 'on' : ''}`} onClick={() => setDryRun(value => !value)} aria-label="切换预览模式" disabled={busy !== null}><span /></button></div></div>
          <div className="recovery-panel"><div className="recovery-copy"><div className="recovery-icon"><Archive size={19} /></div><div><strong>{dryRun ? '预览修复范围' : '同步会话可见性'}</strong><p>{dryRun ? `检查 ${selectedProvider?.name ?? selected} 的 threads 与 local catalog，不修改文件。` : selected !== scan.currentProvider ? '目标必须与 config.toml 当前 provider 一致，避免修复后再次被侧栏过滤。' : writeBlocked ? '检测到活动进程或权限问题；关闭相关进程后重新扫描。' : '写入前创建 SQLite 在线快照；只更新 provider 与本地索引元数据。'}</p></div></div><button className="primary-button" onClick={repair} disabled={busy !== null || (!dryRun && (selected !== scan.currentProvider || writeBlocked))}><Play size={15} fill="currentColor" />{busy === 'repair' ? '处理中' : dryRun ? '预览修复' : '开始同步'}</button></div>
          <div className="safety-line"><LockKeyhole size={14} /><span>不会修改 JSONL、session_index、auth.json、config.toml 或 global state</span><span className="dot" /><span>失败自动恢复 SQLite 快照</span></div>

          <div className="section-heading log-heading"><div><p className="eyebrow">03 / ACTIVITY</p><h2>执行日志</h2></div><button className="text-button" onClick={() => setShowAllLogs(value => !value)}>{showAllLogs ? '收起' : '查看全部'}<ChevronDown size={14} className={showAllLogs ? 'flip' : ''} /></button></div>
          <div className="log-list">{visibleLogs.map((log, index) => <div className="log-row" key={`${log.time}-${index}`}><span className={`log-dot ${log.tone}`} /><time>{log.time}</time><span>{log.text}</span></div>)}</div>
        </section>

        <aside className="side-column">
          <div className="side-block"><div className="side-heading"><h3>操作准备</h3><CircleCheck size={17} /></div><div className="check-row"><span className={`check-icon ${scan.codexHome === '未发现' ? 'warn' : ''}`}>{scan.codexHome === '未发现' ? <CircleAlert size={13} /> : <Check size={13} />}</span><span>CODEX_HOME 已发现</span><small>{scan.codexHome === '未发现' ? 'check' : 'ready'}</small></div><div className="check-row"><span className={`check-icon ${scan.sqlite < 2 ? 'warn' : ''}`}>{scan.sqlite >= 2 ? <Check size={13} /> : <CircleAlert size={13} />}</span><span>{scan.sqlite} 个 SQLite 可读取</span><small>{scan.sqlite >= 2 ? 'ready' : 'check'}</small></div><div className="check-row"><span className={`check-icon ${globalState?.readable && sessionIndex?.readable ? '' : 'warn'}`}>{globalState?.readable && sessionIndex?.readable ? <Check size={13} /> : <CircleAlert size={13} />}</span><span>global state / session index</span><small title={`${globalState?.note ?? '未扫描'} · ${sessionIndex?.note ?? '未扫描'}`}>{globalState?.readable && sessionIndex?.readable ? 'ready' : 'check'}</small></div><div className="check-row"><span className={`check-icon ${writeBlocked ? 'warn' : ''}`}>{writeBlocked ? <CircleAlert size={13} /> : <Check size={13} />}</span><span>进程锁状态</span><small className={writeBlocked ? 'warn-text' : ''} title={scan.lockDetail.activeProcesses.join('、')}>{scan.lock}</small></div></div>
          <div className="side-block backup-block"><div className="side-heading"><h3>备份与回滚</h3><Database size={17} /></div><p>两套 SQLite 在线快照与完整 manifest；JSONL 只登记、不复制。</p><button className="backup-button" onClick={createBackup} disabled={busy !== null || writeBlocked}><ArrowDownToLine size={15} />{busy === 'backup' ? '创建中' : '创建 SQLite 快照'}</button>{scan.lastBackup && <div className="last-backup"><FileClock size={14} /><span>最近备份<strong>{scan.lastBackup}</strong></span></div>}<button className="rollback-button" onClick={rollback} disabled={!scan.lastBackup || busy !== null || writeBlocked}><RotateCcw size={14} />{busy === 'rollback' ? '回滚中' : '回滚最近一次'}</button></div>
          <div className="side-block note-block"><div className="side-heading"><h3>扫描说明</h3><CircleAlert size={17} /></div><p>有效 rollout {scan.validRolloutSessions}/{scan.rolloutSessions}；自动化 {scan.automatedSessions}。远端使用 host 与 SSH/WSL/容器强标记识别，source=vscode 本身不代表远端。session_index 仅作诊断。</p><div className="file-chips"><span><Database size={12} /> state_5.sqlite</span><span><Terminal size={12} /> local_thread_catalog</span><span><FileClock size={12} /> rollout JSONL</span></div></div>
        </aside>
      </div>
    </main>
    {toast && <div className="toast"><CircleCheck size={16} />{toast}<button onClick={() => setToast(null)} aria-label="关闭提示"><X size={14} /></button></div>}
  </div>
}
