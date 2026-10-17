import type { Metadata } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import { MarkStateProvider } from "@/components/mark-state";
import "./globals.css";

const geistSans = Geist({
  variable: "--font-geist-sans",
  subsets: ["latin"],
});

const geistMono = Geist_Mono({
  variable: "--font-geist-mono",
  subsets: ["latin"],
});

export const metadata: Metadata = {
  title: { default: "roll", template: "%s · roll" },
  description: "Serverless infrastructure for everything.",
};

export default function RootLayout({ children }: LayoutProps<"/">) {
  return (
    <html lang="en" className={`${geistSans.variable} ${geistMono.variable} h-full antialiased`}>
      <body className="min-h-full flex flex-col font-sans">
        <MarkStateProvider>{children}</MarkStateProvider>
      </body>
    </html>
  );
}
