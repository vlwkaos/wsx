// Element builder for the terminal view, which drives xterm.js directly.
// Text is always set as text, never parsed as HTML.
export function h(tag, attributes = {}, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value === false || value == null) continue;
    if (name.startsWith("on")) element.addEventListener(name.slice(2), value);
    else if (name === "class") element.classList.add(...value.split(" "));
    else if (name in element && name !== "list") element[name] = value;
    else element.setAttribute(name, value === true ? "" : value);
  }
  add(element, ...children);
  return element;
}

// Optional children are written as `condition ? node : null`; drop the gaps
// here so the DOM never renders the text "null".
const present = (children) => children.flat().filter((child) => child != null && child !== false);
export function add(parent, ...children) {
  parent.append(...present(children));
}
export function fill(parent, ...children) {
  parent.replaceChildren(...present(children));
}
