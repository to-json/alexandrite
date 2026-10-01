n = 100
s = 0
q = 0
i = 1
while i <= n
  s += i
  q += i * i
  i += 1
end
puts s * s - q
