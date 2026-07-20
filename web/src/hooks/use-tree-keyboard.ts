"use client";

import { useCallback, useRef } from "react";
import type { TreeNodeData, FlatTreeNode } from "@/lib/tree-types";

export interface UseTreeKeyboardOptions<T extends TreeNodeData> {
  visibleNodes: FlatTreeNode<T>[];
  flatNodes: FlatTreeNode<T>[];
  focusedId: string | null;
  expandedIds: Set<string>;
  setFocused: (id: string | null) => void;
  toggleExpand: (id: string) => void;
  expand: (id: string) => void;
  collapse: (id: string) => void;
  select: (id: string) => void;
  toggleSelect: (id: string) => void;
  selectRange: (id: string) => void;
  selectAll: () => void;
  selectionMode: "none" | "single" | "multiple";
}

export function useTreeKeyboard<T extends TreeNodeData>(
  options: UseTreeKeyboardOptions<T>
): { onKeyDown: React.KeyboardEventHandler } {
  const optionsRef = useRef(options);
  optionsRef.current = options;

  // Type-ahead buffer
  const typeAheadBuffer = useRef("");
  const typeAheadTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const onKeyDown: React.KeyboardEventHandler = useCallback((e) => {
    const {
      visibleNodes,
      flatNodes,
      focusedId,
      expandedIds,
      setFocused,
      expand,
      collapse,
      select,
      toggleSelect,
      selectRange,
      selectAll,
      selectionMode,
    } = optionsRef.current;

    if (visibleNodes.length === 0) return;

    const currentIndex = focusedId
      ? visibleNodes.findIndex((n) => n.id === focusedId)
      : -1;
    const currentNode =
      currentIndex >= 0 ? visibleNodes[currentIndex] : null;

    switch (e.key) {
      case "ArrowDown": {
        e.preventDefault();
        const nextIndex = Math.min(
          currentIndex + 1,
          visibleNodes.length - 1
        );
        const nextId = visibleNodes[nextIndex].id;
        setFocused(nextId);
        if (e.shiftKey && selectionMode === "multiple") {
          selectRange(nextId);
        }
        break;
      }

      case "ArrowUp": {
        e.preventDefault();
        const prevIndex = Math.max(currentIndex - 1, 0);
        const prevId = visibleNodes[prevIndex].id;
        setFocused(prevId);
        if (e.shiftKey && selectionMode === "multiple") {
          selectRange(prevId);
        }
        break;
      }

      case "ArrowRight": {
        e.preventDefault();
        if (!currentNode) break;
        if (currentNode.isGroup && !expandedIds.has(currentNode.id)) {
          // Expand the node
          expand(currentNode.id);
        } else if (currentNode.isGroup && expandedIds.has(currentNode.id)) {
          // Move to first child
          const nextIndex = currentIndex + 1;
          if (
            nextIndex < visibleNodes.length &&
            visibleNodes[nextIndex].parentId === currentNode.id
          ) {
            setFocused(visibleNodes[nextIndex].id);
          }
        }
        break;
      }

      case "ArrowLeft": {
        e.preventDefault();
        if (!currentNode) break;
        if (currentNode.isGroup && expandedIds.has(currentNode.id)) {
          // Collapse the node
          collapse(currentNode.id);
        } else if (currentNode.parentId !== null) {
          // Move focus to parent
          setFocused(currentNode.parentId);
        }
        break;
      }

      case "Home": {
        e.preventDefault();
        if (visibleNodes.length > 0) {
          const firstId = visibleNodes[0].id;
          setFocused(firstId);
          if (e.shiftKey && selectionMode === "multiple") {
            selectRange(firstId);
          }
        }
        break;
      }

      case "End": {
        e.preventDefault();
        if (visibleNodes.length > 0) {
          const lastId = visibleNodes[visibleNodes.length - 1].id;
          setFocused(lastId);
          if (e.shiftKey && selectionMode === "multiple") {
            selectRange(lastId);
          }
        }
        break;
      }

      case "Enter": {
        e.preventDefault();
        if (currentNode) {
          select(currentNode.id);
        }
        break;
      }

      case " ": {
        e.preventDefault();
        if (currentNode) {
          if (selectionMode === "multiple") {
            toggleSelect(currentNode.id);
          } else {
            select(currentNode.id);
          }
        }
        break;
      }

      case "*": {
        // Expand all siblings of the focused node
        e.preventDefault();
        if (!currentNode) break;
        for (const node of flatNodes) {
          if (
            node.parentId === currentNode.parentId &&
            node.isGroup
          ) {
            expand(node.id);
          }
        }
        break;
      }

      default: {
        // Ctrl/Cmd+A: select all visible nodes
        if (
          e.key === "a" &&
          (e.ctrlKey || e.metaKey) &&
          selectionMode === "multiple"
        ) {
          e.preventDefault();
          selectAll();
          break;
        }

        // Type-ahead: single character
        if (e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey) {
          e.preventDefault();
          clearTimeout(typeAheadTimer.current);
          typeAheadBuffer.current += e.key.toLowerCase();

          typeAheadTimer.current = setTimeout(() => {
            typeAheadBuffer.current = "";
          }, 500);

          // Search from current position forward, wrapping around
          const startIndex = currentIndex + 1;
          for (let i = 0; i < visibleNodes.length; i++) {
            const idx = (startIndex + i) % visibleNodes.length;
            const node = visibleNodes[idx];
            // Use a simple heuristic: check if any string value in data starts with the buffer
            const label = getNodeLabel(node);
            if (label.toLowerCase().startsWith(typeAheadBuffer.current)) {
              setFocused(node.id);
              break;
            }
          }
        }
        break;
      }
    }
  }, []);

  return { onKeyDown };
}

/**
 * Best-effort label extraction from node data for type-ahead.
 */
function getNodeLabel<T extends TreeNodeData>(node: FlatTreeNode<T>): string {
  const data = node.data as Record<string, unknown>;
  // Try common label fields
  for (const key of ["label", "name", "title", "text"]) {
    if (typeof data[key] === "string") return data[key];
  }
  // Fallback to first string value
  for (const value of Object.values(data)) {
    if (typeof value === "string") return value;
  }
  return node.id;
}
