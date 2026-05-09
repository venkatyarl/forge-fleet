import React from "react";

interface GpuNode {
  name: string;
  gpu_util: number; // 0-100
  vram_used_gb: number;
  vram_total_gb: number;
  temp_c: number;
}

interface GpuHeatmapProps {
  nodes: GpuNode[];
}

function heatColor(value: number): string {
  // 0% = green, 50% = yellow, 100% = red
  if (value < 50) {
    const t = value / 50;
    return `rgb(${Math.round(t * 255)}, 255, 0)`;
  }
  const t = (value - 50) / 50;
  return `rgb(255, ${Math.round((1 - t) * 255)}, 0)`;
}

export const GpuHeatmap: React.FC<GpuHeatmapProps> = ({ nodes }) => {
  return (
    <div className="gpu-heatmap">
      <h3 className="text-sm font-semibold mb-2">GPU Utilization Heatmap</h3>
      <div className="grid grid-cols-3 gap-2">
        {nodes.map((n) => {
          const vramPct = (n.vram_used_gb / n.vram_total_gb) * 100;
          return (
            <div
              key={n.name}
              className="rounded p-2 text-xs text-black font-medium"
              style={{ backgroundColor: heatColor(n.gpu_util) }}
              title={`${n.name}: ${n.gpu_util}% GPU, ${n.vram_used_gb.toFixed(1)}/${n.vram_total_gb}GB VRAM, ${n.temp_c}°C`}
            >
              <div className="truncate font-bold">{n.name}</div>
              <div>GPU {n.gpu_util.toFixed(0)}%</div>
              <div>VRAM {vramPct.toFixed(0)}%</div>
              <div>{n.temp_c}°C</div>
            </div>
          );
        })}
      </div>
    </div>
  );
};

export default GpuHeatmap;
