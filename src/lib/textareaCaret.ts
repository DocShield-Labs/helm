const MIRRORED_PROPERTIES = [
  'fontFamily',
  'fontSize',
  'fontStyle',
  'fontWeight',
  'letterSpacing',
  'lineHeight',
  'paddingBottom',
  'paddingLeft',
  'paddingRight',
  'paddingTop',
  'tabSize',
  'textAlign',
  'textIndent',
  'textTransform',
  'wordSpacing',
] as const

/** Viewport rect of a textarea character position. A short-lived DOM
 * mirror is the only reliable way to account for wrapping, proportional
 * fonts, padding, and textarea scroll position across WebKit versions. */
export function textareaCaretRect(textarea: HTMLTextAreaElement, position: number): DOMRect {
  const textareaRect = textarea.getBoundingClientRect()
  const computed = window.getComputedStyle(textarea)
  const viewport = document.createElement('div')
  const content = document.createElement('div')
  const marker = document.createElement('span')

  Object.assign(viewport.style, {
    position: 'fixed',
    visibility: 'hidden',
    pointerEvents: 'none',
    overflow: 'hidden',
    top: `${textareaRect.top}px`,
    left: `${textareaRect.left}px`,
    width: `${textareaRect.width}px`,
    height: `${textareaRect.height}px`,
  })
  Object.assign(content.style, {
    boxSizing: 'border-box',
    width: `${textareaRect.width}px`,
    minHeight: `${textareaRect.height}px`,
    whiteSpace: 'pre-wrap',
    overflowWrap: 'break-word',
    transform: `translate(${-textarea.scrollLeft}px, ${-textarea.scrollTop}px)`,
  })
  for (const property of MIRRORED_PROPERTIES) content.style[property] = computed[property]

  content.textContent = textarea.value.slice(0, position)
  marker.textContent = '\u200b'
  content.append(marker)
  viewport.append(content)
  document.body.append(viewport)
  const rect = marker.getBoundingClientRect()
  viewport.remove()
  return rect
}
