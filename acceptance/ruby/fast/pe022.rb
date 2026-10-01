names = File.read("fixtures/names.txt").delete('"').split(",").sort!
t = 0
i = 0
n = names.size
while i < n
  name = names[i]
  t += (i + 1) * (name.sum(0) - 64 * name.bytesize)
  i += 1
end
puts t
