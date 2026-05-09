import React from "react";

interface Task {
  id: string;
  title: string;
  status: "todo" | "in_progress" | "review" | "done";
  priority: "low" | "medium" | "high";
  assignee?: string;
}

interface TaskKanbanProps {
  tasks: Task[];
  onMove?: (id: string, to: Task["status"]) => void;
}

const COLUMNS: { key: Task["status"]; label: string; color: string }[] = [
  { key: "todo", label: "To Do", color: "bg-slate-700" },
  { key: "in_progress", label: "In Progress", color: "bg-blue-700" },
  { key: "review", label: "Review", color: "bg-amber-700" },
  { key: "done", label: "Done", color: "bg-emerald-700" },
];

export const TaskKanban: React.FC<TaskKanbanProps> = ({ tasks, onMove }) => {
  return (
    <div className="task-kanban">
      <h3 className="text-sm font-semibold mb-2">Task Board</h3>
      <div className="grid grid-cols-4 gap-3">
        {COLUMNS.map((col) => (
          <div
            key={col.key}
            className={`rounded p-2 min-h-[120px] ${col.color} bg-opacity-20`}
          >
            <div className="text-xs font-bold mb-2 uppercase tracking-wider">
              {col.label}
            </div>
            {tasks
              .filter((t) => t.status === col.key)
              .map((t) => (
                <div
                  key={t.id}
                  className="rounded bg-slate-800 p-2 mb-2 text-xs cursor-move hover:bg-slate-700 transition"
                  draggable
                  onDragEnd={() => onMove?.(t.id, col.key)}
                >
                  <div className="font-medium truncate">{t.title}</div>
                  <div className="flex justify-between mt-1 text-slate-400">
                    <span
                      className={`px-1 rounded ${
                        t.priority === "high"
                          ? "bg-red-900 text-red-200"
                          : t.priority === "medium"
                          ? "bg-amber-900 text-amber-200"
                          : "bg-slate-600 text-slate-200"
                      }`}
                    >
                      {t.priority}
                    </span>
                    {t.assignee && <span>@{t.assignee}</span>}
                  </div>
                </div>
              ))}
          </div>
        ))}
      </div>
    </div>
  );
};

export default TaskKanban;
