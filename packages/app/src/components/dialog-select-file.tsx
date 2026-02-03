import type { Session } from "@opencode-ai/sdk/v2/client"
import { Avatar } from "@opencode-ai/ui/avatar"
import { useDialog } from "@opencode-ai/ui/context/dialog"
import { Dialog } from "@opencode-ai/ui/dialog"
import { FileIcon } from "@opencode-ai/ui/file-icon"
import { Icon } from "@opencode-ai/ui/icon"
import { Keybind } from "@opencode-ai/ui/keybind"
import { List } from "@opencode-ai/ui/list"
import { getDirectory, getFilename } from "@opencode-ai/util/path"
import { useNavigate, useParams } from "@solidjs/router"
import { createMemo, createSignal, Match, onCleanup, Show, Switch } from "solid-js"
import { base64Encode } from "@opencode-ai/util/encode"
import { formatKeybind, useCommand, type CommandOption } from "@/context/command"
import { useFile } from "@/context/file"
import { useGlobalSync } from "@/context/global-sync"
import { useLanguage } from "@/context/language"
import { getAvatarColors, useLayout, type LocalProject } from "@/context/layout"
import { useServer } from "@/context/server"
import { useSettings } from "@/context/settings"

type EntryType = "command" | "file" | "project" | "session"

type Entry = {
  id: string
  type: EntryType
  title: string
  description?: string
  keybind?: string
  category: string
  option?: CommandOption
  path?: string
}

type DialogSelectFileMode = "all" | "files"

export function DialogSelectFile(props: { mode?: DialogSelectFileMode; onOpenFile?: (path: string) => void }) {
  const command = useCommand()
  const language = useLanguage()
  const layout = useLayout()
  const globalSync = useGlobalSync()
  const server = useServer()
  const settings = useSettings()
  const file = useFile()
  const dialog = useDialog()
  const nav = useNavigate()
  const params = useParams()
  const filesOnly = () => props.mode === "files"
  const sessionKey = createMemo(() => `${params.dir}${params.id ? "/" + params.id : ""}`)
  const tabs = createMemo(() => layout.tabs(sessionKey))
  const state = { cleanup: undefined as (() => void) | void, committed: false }
  const [grouped, setGrouped] = createSignal(false)
  const common = [
    "session.new",
    "workspace.new",
    "session.previous",
    "session.next",
    "terminal.toggle",
    "review.toggle",
  ]
  const limit = 5
  const groupOrder = createMemo(() => {
    const order = [
      language.t("palette.group.projects"),
      language.t("palette.group.sessions"),
      language.t("palette.group.commands"),
      language.t("palette.group.files"),
    ]
    return new Map(order.map((name, i) => [name, i]))
  })

  const allowed = createMemo(() => {
    if (filesOnly()) return []
    return command.options.filter(
      (option) => !option.disabled && !option.id.startsWith("suggested.") && option.id !== "file.open",
    )
  })

  const commandItem = (option: CommandOption): Entry => ({
    id: "command:" + option.id,
    type: "command",
    title: option.title,
    description: option.description,
    keybind: option.keybind,
    category: language.t("palette.group.commands"),
    option,
  })

  const fileItem = (path: string): Entry => ({
    id: "file:" + path,
    type: "file",
    title: path,
    category: language.t("palette.group.files"),
    path,
  })

  const projectItem = (project: LocalProject): Entry => ({
    id: "project:" + project.worktree,
    type: "project",
    title: project.name || getFilename(project.worktree),
    description: project.worktree,
    category: language.t("palette.group.projects"),
    path: project.worktree,
  })

  const sessionItem = (session: Session, project: LocalProject): Entry => ({
    id: "session:" + session.id,
    type: "session",
    title: session.title,
    description: project.name || getFilename(project.worktree),
    category: language.t("palette.group.sessions"),
    path: `${session.directory}\0${session.id}`,
  })

  const projectList = createMemo(() => layout.projects.list())
  const projectLookup = createMemo(() => new Map(projectList().map((p) => [p.worktree, p])))
  const projects = createMemo(() => projectList().map(projectItem))
  const sessions = createMemo(() => {
    const result: Entry[] = []
    for (const project of projectList()) {
      const [store] = globalSync.child(project.worktree, { bootstrap: false })
      for (const session of store.session) {
        if (session.parentID) continue
        if (session.time?.archived) continue
        result.push(sessionItem(session, project))
      }
    }
    return result
  })
  const list = createMemo(() => allowed().map(commandItem))

  const picks = createMemo(() => {
    const all = allowed()
    const order = new Map(common.map((id, index) => [id, index]))
    const picked = all.filter((option) => order.has(option.id))
    const base = picked.length ? picked : all.slice(0, limit)
    const sorted = picked.length ? [...base].sort((a, b) => (order.get(a.id) ?? 0) - (order.get(b.id) ?? 0)) : base
    return sorted.map(commandItem)
  })

  const recent = createMemo(() => {
    const all = tabs().all()
    const active = tabs().active()
    const order = active ? [active, ...all.filter((item) => item !== active)] : all
    const seen = new Set<string>()
    const items: Entry[] = []

    for (const item of order) {
      const path = file.pathFromTab(item)
      if (!path) continue
      if (seen.has(path)) continue
      seen.add(path)
      items.push(fileItem(path))
    }

    return items.slice(0, limit)
  })

  const root = createMemo(() => {
    const nodes = file.tree.children("")
    const paths = nodes
      .filter((node) => node.type === "file")
      .map((node) => node.path)
      .sort((a, b) => a.localeCompare(b))
    return paths.slice(0, limit).map(fileItem)
  })

  const unique = (items: Entry[]) => {
    const seen = new Set<string>()
    const out: Entry[] = []
    for (const item of items) {
      if (seen.has(item.id)) continue
      seen.add(item.id)
      out.push(item)
    }
    return out
  }

  const items = async (text: string) => {
    const query = text.trim()
    setGrouped(query.length > 0)

    if (!query && filesOnly()) {
      const loaded = file.tree.state("")?.loaded
      const pending = loaded ? Promise.resolve() : file.tree.list("")
      const next = unique([...recent(), ...root()])

      if (loaded || next.length > 0) {
        void pending
        return next
      }

      await pending
      return unique([...recent(), ...root()])
    }

    if (!query) return [...picks(), ...recent()]

    if (filesOnly()) {
      const files = await file.searchFiles(query)
      return files.map(fileItem)
    }
    const files = await file.searchFiles(query)
    const entries = files.map(fileItem)
    return [
      ...(settings.palette.showProjects() ? projects() : []),
      ...(settings.palette.showSessions() ? sessions() : []),
      ...list(),
      ...entries,
    ]
  }

  const handleMove = (item: Entry | undefined) => {
    state.cleanup?.()
    if (!item) return
    if (item.type !== "command") return
    state.cleanup = item.option?.onHighlight?.()
  }

  const open = (path: string) => {
    const value = file.tab(path)
    tabs().open(value)
    file.load(path)
    layout.fileTree.open()
    layout.fileTree.setTab("all")
    props.onOpenFile?.(path)
  }

  const handleSelect = (item: Entry | undefined) => {
    if (!item) return
    state.committed = true
    state.cleanup = undefined
    dialog.close()

    if (item.type === "command") {
      item.option?.onSelect?.("palette")
      return
    }

    if (item.type === "project" && item.path) {
      server.projects.touch(item.path)
      nav(`/${base64Encode(item.path)}`)
      return
    }

    if (item.type === "session" && item.path) {
      const [directory, sessionId] = item.path.split("\0")
      server.projects.touch(directory)
      nav(`/${base64Encode(directory)}/session/${sessionId}`)
      return
    }

    if (!item.path) return
    open(item.path)
  }

  onCleanup(() => {
    if (state.committed) return
    state.cleanup?.()
  })

  return (
    <Dialog class="pt-3 pb-0 !max-h-[480px]" transition>
      <List
        search={{
          placeholder: filesOnly()
            ? language.t("session.header.searchFiles")
            : language.t("palette.search.placeholder"),
          autofocus: true,
          hideIcon: true,
        }}
        emptyMessage={language.t("palette.empty")}
        loadingMessage={language.t("common.loading")}
        items={items}
        key={(item) => item.id}
        filterKeys={["title", "description", "category"]}
        groupBy={(item) => item.category}
        sortGroupsBy={(a, b) => (groupOrder().get(a.category) ?? 99) - (groupOrder().get(b.category) ?? 99)}
        onMove={handleMove}
        onSelect={handleSelect}
      >
        {(item) => (
          <Switch>
            <Match when={item.type === "command"}>
              <div class="w-full flex items-center justify-between gap-4">
                <div class="flex items-center gap-2 min-w-0">
                  <span class="text-14-regular text-text-strong whitespace-nowrap">{item.title}</span>
                  <Show when={item.description}>
                    <span class="text-14-regular text-text-weak truncate">{item.description}</span>
                  </Show>
                </div>
                <Show when={item.keybind}>
                  <Keybind class="rounded-[4px]">{formatKeybind(item.keybind ?? "")}</Keybind>
                </Show>
              </div>
            </Match>
            <Match when={item.type === "project"}>
              {(() => {
                const project = projectLookup().get(item.path ?? "")
                const icon = project?.icon as { override?: string; color?: string } | undefined
                return (
                  <div class="w-full flex items-center justify-between rounded-md pl-1">
                    <div class="flex items-center gap-x-3 grow min-w-0">
                      <Avatar
                        fallback={item.title}
                        src={icon?.override}
                        {...getAvatarColors(icon?.color)}
                        size="small"
                        class="shrink-0"
                      />
                      <div class="flex items-center gap-1.5 text-14-regular min-w-0">
                        <span class="text-text-strong whitespace-nowrap">{item.title}</span>
                        <span class="text-text-weak truncate min-w-0">{item.description}</span>
                      </div>
                    </div>
                  </div>
                )
              })()}
            </Match>
            <Match when={item.type === "session"}>
              <div class="w-full flex items-center justify-between rounded-md pl-1">
                <div class="flex items-center gap-x-3 grow min-w-0">
                  <Icon name="comment" size="small" class="shrink-0 text-icon-weak" />
                  <div class="flex items-center gap-1.5 text-14-regular min-w-0">
                    <span class="text-text-strong whitespace-nowrap">{item.title}</span>
                    <span class="text-text-weak truncate min-w-0">{item.description}</span>
                  </div>
                </div>
              </div>
            </Match>
            <Match when={item.type === "file"}>
              <div class="w-full flex items-center justify-between rounded-md pl-1">
                <div class="flex items-center gap-x-3 grow min-w-0">
                  <FileIcon node={{ path: item.path ?? "", type: "file" }} class="shrink-0 size-4" />
                  <div class="flex items-center text-14-regular">
                    <span class="text-text-weak whitespace-nowrap overflow-hidden overflow-ellipsis truncate min-w-0">
                      {getDirectory(item.path ?? "")}
                    </span>
                    <span class="text-text-strong whitespace-nowrap">{getFilename(item.path ?? "")}</span>
                  </div>
                </div>
              </div>
            </Match>
          </Switch>
        )}
      </List>
    </Dialog>
  )
}
