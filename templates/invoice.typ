// Kwartje invoice and quote template. All values arrive pre-formatted (Dutch) in
// data.json; strings are inserted as plain text, never as markup.
#let d = json("data.json")
#let muted = luma(110)

#set document(title: d.invoice.title, author: d.company.name)
#set text(font: "Libertinus Serif", size: 10pt, lang: "nl")
#set page(
  paper: "a4",
  margin: (x: 2cm, top: 1.8cm, bottom: 2.4cm),
  footer: context [
    #set text(8pt, fill: muted)
    #d.company.name · KvK #d.company.kvk · Btw-id #d.company.btw_id · IBAN #d.company.iban
    #h(1fr) Pagina #counter(page).display() van #counter(page).final().first()
  ],
)
#set table(stroke: none, inset: (x: 4pt, y: 5pt))

#grid(
  columns: (1fr, auto),
  align: (left + top, right + top),
  if d.logo != none { image(d.logo, height: 1.6cm) } else { text(16pt, weight: "bold", d.company.name) },
  [
    #set par(spacing: 0.4em)
    #strong(d.company.name) \
    #for line in d.company.address [#line \ ]
    #if d.company.email != "" [#d.company.email \ ]
    #if d.company.phone != "" [#d.company.phone]
  ],
)

#v(1.2cm)
#grid(
  columns: (1fr, auto),
  [
    #set par(spacing: 0.4em)
    #strong(d.client.name) \
    #if d.client.contact != "" [t.a.v. #d.client.contact \ ]
    #for line in d.client.address [#line \ ]
    #if d.client.btw_id != "" [Btw-id #d.client.btw_id]
  ],
  table(
    columns: 2,
    align: (left, right),
    ..d.meta.enumerate().map(((i, row)) => if i == 0 { (row.at(0), strong(row.at(1))) } else { (row.at(0), row.at(1)) }).flatten(),
  ),
)

#v(0.8cm)
#text(18pt, weight: "bold", d.invoice.kind_label)
#v(0.2cm)
#if d.invoice.intro != "" [#d.invoice.intro #v(0.3cm)]

#table(
  columns: (1fr, auto, auto, auto, auto),
  align: (left, right, right, right, right),
  table.hline(stroke: 0.6pt),
  table.header([*Omschrijving*], [*Aantal*], [*Prijs*], [*Btw*], [*Bedrag*]),
  table.hline(stroke: 0.6pt),
  ..d.lines.map(l => (l.description, l.quantity, l.price, l.vat, l.total)).flatten(),
  table.hline(stroke: 0.6pt),
)

#align(right, table(
  columns: 2,
  align: (left, right),
  [Subtotaal], d.subtotal,
  ..d.vat_groups.map(g => ([Btw #g.rate over #g.base], g.vat)).flatten(),
  table.hline(stroke: 0.6pt),
  strong[Totaal], strong(d.total),
))

#v(0.6cm)
#d.closing
#if d.invoice.footer != "" [#v(0.3cm) #d.invoice.footer]

#if d.spec != none [
  #pagebreak()
  #text(14pt, weight: "bold")[Urenspecificatie bij #d.invoice.number]
  #v(0.3cm)
  #table(
    columns: (auto, auto, 1fr, auto, auto),
    align: (left, left, left, right, right),
    table.hline(stroke: 0.6pt),
    table.header([*Datum*], [*Taak*], [*Omschrijving*], [*Duur*], [*Gefactureerd*]),
    table.hline(stroke: 0.6pt),
    ..d.spec.map(r => (r.date, r.task, r.description, r.duration, r.billed)).flatten(),
    table.hline(stroke: 0.6pt),
    [], [], strong[Totaal], strong(d.spec_total), strong(d.spec_billed_total),
  )
  #if d.spec_total != d.spec_billed_total [
    #text(8pt, fill: muted)[Gefactureerd = per regel naar boven afgerond volgens de afspraak.]
  ]
]
