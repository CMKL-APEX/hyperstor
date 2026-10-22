/** @type {import('tailwindcss').Config} */
export default {
  // Light only, as @glidex/ui-kit's dialogs are: `dark:` classes stay
  // inert until a theme switch sets the class.
  darkMode: "class",
  // The kit's classes too: it is source, built with the app.
  content: ["./index.html", "./src/**/*.{js,ts,jsx,tsx}", "../ui-kit/src/**/*.{js,ts,jsx,tsx}"],
  theme: {
    extend: {},
  },
  plugins: [],
};
