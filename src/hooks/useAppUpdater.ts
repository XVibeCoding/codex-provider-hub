import { useCallback, useRef, useState } from 'react'
import type { Update } from '@tauri-apps/plugin-updater'

export type AppUpdateStatus =
  | 'idle'
  | 'checking'
  | 'upToDate'
  | 'available'
  | 'downloading'
  | 'installing'
  | 'error'

export type AppUpdateProgress = {
  downloaded: number
  total: number | null
  percent: number | null
}

const EMPTY_PROGRESS: AppUpdateProgress = {
  downloaded: 0,
  total: null,
  percent: null,
}

function readableUpdateError(error: unknown) {
  const message = String(error).replace(/^Error:\s*/i, '')
  if (/404|not found|valid release json|release json|latest\.json/i.test(message)) {
    return '仓库暂未发布可在线安装的正式版本。首次发布后，可直接在这里完成更新。'
  }
  if (/timed?\s*out|network|connection|dns|request|fetch/i.test(message)) {
    return '暂时无法连接更新服务器，请检查网络后重试。'
  }
  if (/signature|verify|public key/i.test(message)) {
    return '更新包签名校验未通过，已停止安装。请从项目仓库确认正式版本。'
  }
  return message || '检查更新失败，请稍后重试。'
}

export function useAppUpdater() {
  const updateRef = useRef<Update | null>(null)
  const [status, setStatus] = useState<AppUpdateStatus>('idle')
  const [latestVersion, setLatestVersion] = useState<string | null>(null)
  const [releaseNotes, setReleaseNotes] = useState<string | null>(null)
  const [progress, setProgress] = useState<AppUpdateProgress>(EMPTY_PROGRESS)
  const [error, setError] = useState<string | null>(null)

  const checkForUpdates = useCallback(async () => {
    if (status === 'checking' || status === 'downloading' || status === 'installing') return
    setStatus('checking')
    setError(null)
    setProgress(EMPTY_PROGRESS)
    try {
      if (updateRef.current) {
        await updateRef.current.close().catch(() => undefined)
        updateRef.current = null
      }
      const { check } = await import('@tauri-apps/plugin-updater')
      const update = await check({ timeout: 15_000 })
      if (!update) {
        setLatestVersion(null)
        setReleaseNotes(null)
        setStatus('upToDate')
        return
      }
      updateRef.current = update
      setLatestVersion(update.version)
      setReleaseNotes(update.body?.trim() || null)
      setStatus('available')
    } catch (caught) {
      setError(readableUpdateError(caught))
      setStatus('error')
    }
  }, [status])

  const installUpdate = useCallback(async () => {
    const update = updateRef.current
    if (!update || status === 'downloading' || status === 'installing') return
    if (import.meta.env.DEV) {
      setError('当前是开发调试版本，不会覆盖正在运行的程序。请从正式安装版执行自动更新。')
      setStatus('error')
      return
    }

    let downloaded = 0
    let total: number | null = null
    setError(null)
    setProgress(EMPTY_PROGRESS)
    setStatus('downloading')
    try {
      await update.downloadAndInstall(event => {
        if (event.event === 'Started') {
          downloaded = 0
          total = event.data.contentLength ?? null
          setProgress({ downloaded, total, percent: total ? 0 : null })
          return
        }
        if (event.event === 'Progress') {
          downloaded += event.data.chunkLength
          const percent = total ? Math.min(100, Math.round((downloaded / total) * 100)) : null
          setProgress({ downloaded, total, percent })
          return
        }
        setProgress(current => ({ ...current, percent: 100 }))
        setStatus('installing')
      }, { timeout: 5 * 60_000 })

      setStatus('installing')
      const { relaunch } = await import('@tauri-apps/plugin-process')
      await relaunch()
    } catch (caught) {
      setError(readableUpdateError(caught))
      setStatus('error')
    }
  }, [status])

  return {
    status,
    latestVersion,
    releaseNotes,
    progress,
    error,
    checkForUpdates,
    installUpdate,
  }
}
