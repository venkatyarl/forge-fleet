import { useEffect } from 'react'
import { useNavigate } from 'react-router-dom'
import { Command } from 'cmdk'
import { Search } from 'lucide-react'
import { useUIStore } from '../app/store'
import { COMMANDS, COMMAND_CATEGORIES } from '../app/commands'
import { cn } from '../lib/utils'

export function CommandPalette() {
  const open = useUIStore((s) => s.commandPaletteOpen)
  const setOpen = useUIStore((s) => s.setCommandPaletteOpen)
  const navigate = useNavigate()

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
        e.preventDefault()
        setOpen(!open)
      }
      if (e.key === 'Escape') setOpen(false)
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [open, setOpen])

  const execute = (item: (typeof COMMANDS)[number]) => {
    if (item.path) navigate(item.path)
    if (item.action) item.action()
    setOpen(false)
  }

  return (
    <Command.Dialog
      open={open}
      onOpenChange={setOpen}
      label="Command palette"
      className="fixed inset-0 z-50 flex items-start justify-center pt-[20vh]"
    >
      <div className="fixed inset-0 bg-black/60" onClick={() => setOpen(false)} />
      <div className="relative w-full max-w-lg overflow-hidden rounded-xl border border-border bg-elevated shadow-2xl shadow-black/50">
        <div className="flex items-center border-b border-border px-4">
          <Search className="h-5 w-5 text-dim" />
          <Command.Input
            aria-label="Search commands"
            placeholder="Search commands, pages..."
            className="flex-1 bg-transparent px-3 py-3 text-sm text-foreground outline-hidden placeholder:text-dim"
          />
          <kbd className="rounded-sm border border-border-subtle bg-panel px-1.5 py-0.5 text-xs text-dim">esc</kbd>
        </div>

        <Command.List className="max-h-72 overflow-y-auto p-2">
          <Command.Empty className="px-3 py-6 text-center text-sm text-dim">
            No commands found.
          </Command.Empty>

          {COMMAND_CATEGORIES.map((category) => (
            <Command.Group
              key={category}
              heading={category}
              className="text-xs font-semibold uppercase tracking-wider text-dim"
            >
              {COMMANDS.filter((c) => c.category === category).map((item) => (
                <Command.Item
                  key={item.id}
                  onSelect={() => execute(item)}
                  className={cn(
                    'flex w-full cursor-pointer items-center justify-between rounded-lg px-3 py-2 text-sm text-muted',
                    'aria-selected:bg-primary-subtle aria-selected:text-primary'
                  )}
                >
                  <span>{item.label}</span>
                  {item.shortcut && (
                    <kbd className="text-xs text-dim">{item.shortcut}</kbd>
                  )}
                </Command.Item>
              ))}
            </Command.Group>
          ))}
        </Command.List>

        <div className="border-t border-border px-4 py-2 text-xs text-dim">
          ↑↓ navigate · ↵ select · esc close
        </div>
      </div>
    </Command.Dialog>
  )
}
