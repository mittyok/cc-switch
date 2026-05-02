import { useState, useCallback } from "react";
import { useTranslation } from "react-i18next";
import { Plus, Trash2, ArrowRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

interface ModelMappingEditorProps {
  value: Record<string, string>;
  onChange: (mapping: Record<string, string>) => void;
}

export function ModelMappingEditor({
  value,
  onChange,
}: ModelMappingEditorProps) {
  const { t } = useTranslation();
  const [newFrom, setNewFrom] = useState("");
  const [newTo, setNewTo] = useState("");

  const entries = Object.entries(value);

  const handleAdd = () => {
    const from = newFrom.trim();
    const to = newTo.trim();
    if (!from || !to) return;
    onChange({ ...value, [from]: to });
    setNewFrom("");
    setNewTo("");
  };

  const handleRemove = (key: string) => {
    const next = { ...value };
    delete next[key];
    onChange(next);
  };

  /** Update the "from" key of an existing mapping */
  const handleFromChange = useCallback(
    (oldKey: string, newKey: string) => {
      if (newKey === oldKey) return;
      // Rebuild preserving order, replacing the old key with the new one
      const next: Record<string, string> = {};
      for (const [k, v] of Object.entries(value)) {
        if (k === oldKey) {
          next[newKey] = v;
        } else {
          next[k] = v;
        }
      }
      onChange(next);
    },
    [value, onChange],
  );

  /** Update the "to" value of an existing mapping */
  const handleToChange = useCallback(
    (key: string, newValue: string) => {
      onChange({ ...value, [key]: newValue });
    },
    [value, onChange],
  );

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      handleAdd();
    }
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <Label className="text-sm font-medium">
          {t("providerForm.modelMapping", {
            defaultValue: "模型名称映射",
          })}
        </Label>
      </div>
      <p className="text-xs text-muted-foreground">
        {t("providerForm.modelMappingHint", {
          defaultValue:
            "将客户端请求中的模型名称映射为上游 API 实际使用的模型名称（代理转发时自动替换）",
        })}
      </p>

      {/* Existing mappings — directly editable */}
      {entries.length > 0 && (
        <div className="space-y-2">
          {entries.map(([from, to]) => (
            <MappingRow
              key={from}
              fromValue={from}
              toValue={to}
              onFromCommit={(newKey) => handleFromChange(from, newKey)}
              onToChange={(newVal) => handleToChange(from, newVal)}
              onRemove={() => handleRemove(from)}
            />
          ))}
        </div>
      )}

      {/* Add new mapping */}
      <div className="flex items-center gap-2">
        <Input
          value={newFrom}
          onChange={(e) => setNewFrom(e.target.value)}
          onKeyDown={onKeyDown}
          placeholder={t("providerForm.modelMappingFrom", {
            defaultValue: "原始模型名",
          })}
          className="flex-1 font-mono text-sm"
        />
        <ArrowRight className="h-4 w-4 shrink-0 text-muted-foreground" />
        <Input
          value={newTo}
          onChange={(e) => setNewTo(e.target.value)}
          onKeyDown={onKeyDown}
          placeholder={t("providerForm.modelMappingTo", {
            defaultValue: "目标模型名",
          })}
          className="flex-1 font-mono text-sm"
        />
        <Button
          type="button"
          variant="outline"
          size="icon"
          className="h-9 w-9 shrink-0"
          onClick={handleAdd}
          disabled={!newFrom.trim() || !newTo.trim()}
        >
          <Plus className="h-4 w-4" />
        </Button>
      </div>
    </div>
  );
}

/* ------------------------------------------------------------------ */
/*  Inline-editable row for a single mapping entry                     */
/* ------------------------------------------------------------------ */

interface MappingRowProps {
  fromValue: string;
  toValue: string;
  onFromCommit: (newKey: string) => void;
  onToChange: (newValue: string) => void;
  onRemove: () => void;
}

function MappingRow({
  fromValue,
  toValue,
  onFromCommit,
  onToChange,
  onRemove,
}: MappingRowProps) {
  // We keep a local draft for the "from" key so we only commit on blur / Enter
  // (changing the key on every keystroke would constantly recreate the object).
  const [localFrom, setLocalFrom] = useState(fromValue);

  const commitFrom = () => {
    const trimmed = localFrom.trim();
    if (!trimmed) {
      // Revert if the user cleared the field
      setLocalFrom(fromValue);
      return;
    }
    if (trimmed !== fromValue) {
      onFromCommit(trimmed);
    }
  };

  const handleFromKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      (e.target as HTMLInputElement).blur();
    }
  };

  const handleToKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      (e.target as HTMLInputElement).blur();
    }
  };

  return (
    <div className="flex items-center gap-2">
      <Input
        value={localFrom}
        onChange={(e) => setLocalFrom(e.target.value)}
        onBlur={commitFrom}
        onKeyDown={handleFromKeyDown}
        className="flex-1 font-mono text-sm"
      />
      <ArrowRight className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
      <Input
        value={toValue}
        onChange={(e) => onToChange(e.target.value)}
        onKeyDown={handleToKeyDown}
        className="flex-1 font-mono text-sm"
      />
      <Button
        type="button"
        variant="ghost"
        size="icon"
        className="h-7 w-7 shrink-0 text-destructive hover:text-destructive"
        onClick={onRemove}
      >
        <Trash2 className="h-3.5 w-3.5" />
      </Button>
    </div>
  );
}
