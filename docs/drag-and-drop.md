# Drag and drop

The [pinned Rune revision](https://github.com/anomalyco/rune/commit/f1b079ccad541008073e73d0987b876f4c99c322) includes native drag sources, [Wayland drag-out](https://github.com/anomalyco/rune/pull/2408), and [Windows drop-target improvements](https://github.com/anomalyco/rune/pull/2622). The dependency update does not add new Shuvcode gestures on its own.

## Already wired in the desktop client

- **Drop into chat.** Drop a file or image from the system file manager onto a session with a composer, or drag an image from the timeline onto the composer. It goes into the draft, not straight to chat. Supported images and PDFs can attach inline; other files attach by path, with an upload first when the server runs on another machine.
- **Drag from Files or Review.** A file verified on the local disk can be dragged to another app. Drop it back into the composer to insert an `@file` mention instead of uploading it.
- **Drag a diff header out.** The header exports that file’s changes as a `.patch` or plain text, not the working-tree file.

Rune supports native drag-out on macOS, Windows, and Wayland at this pin, but not on X11. Those are engine capabilities, not a claim that each Shuvcode gesture has been verified on each platform. Browser drops need separate verification.

## Next steps

1. **Verify and refine composer drops.** Manually check multi-file and image drops on macOS, Windows, and Wayland, with both local and remote servers. Check that the drop indicator follows what the composer can accept, rejected items have a clear explanation, and cancelling leaves the draft unchanged. Keep the attachment picker as the non-drag alternative.
2. **Make file references work without a local copy.** Let remote and browser workspaces drag a file reference into the composer as an `@file` mention. Keep that in-app gesture separate from exporting file bytes to another app, and retain the keyboard mention flow.
3. **Make Review drag intent clear.** Preserve patch export from the diff header, and give dragging a file into chat an explicit mention target. Keep the file, mention, and patch gestures distinct so a drop never applies a patch or moves a workspace file.
