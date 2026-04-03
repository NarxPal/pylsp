import a
from a import greet
from a import User as Person
from a import value

a
greet()
x = greet
Person
value

f, b = (1, 2)
print(f)
print(b)

s, (y, z) = (1, (2, 3))
print(y)
print(z)

[p, q] = [1, 2]
print(p)
print(q)


def greet(name: str) -> str:
    message = f"hellow, {name}"
    return message

class User:
    def __init__(self, username: str) -> None:
        self.username = username

    def display_name(self) -> str:
        return self.username.title()


class Account:
    def __init__(self, id: int):
        self.id = id


def display_name():
    print("hello")



name = "global"

def outer(name):
    x = 1

    def inner(name):
        y = name
        return y

    return name




user = User("narendra")
result = greet(user.display_name())
print(result)
print(user)
print(name)
display_name()